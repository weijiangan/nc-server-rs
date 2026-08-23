//! MKCOL / MOVE / COPY — the structural write path.
//!
//! Each entry point mirrors its PHP `View` counterpart: mutate the disk, move
//! or clone the `oc_filecache` row(s), then run the same `Updater` side
//! effects PHP runs (mimetype recomputation on extension change, version
//! relocation, custom-property repathing, and the source/target chain
//! propagation).

use dav_server::fs::FsError;
use sqlx::Row;
use tracing::warn;

use crate::filesystem::{blocking, io_to_fs};
use crate::path_utils::{extension, is_trash_extension};
use crate::path_utils::{new_etag, parent_fc_path};
use crate::row;
use crate::NcFileSystem;
use nc_db::now_secs;
use nc_db::{db_dispatch, db_execute};

impl NcFileSystem {
    /// exists on disk but not in the filecache) fails with NotFound.
    ///
    /// Deviation: PHP does NOT call `createParentDirectories()` from chunked
    /// upload v2 assembly, so chunked uploads to paths with a non-existent
    /// parent fail.  Rust calls this uniformly from all write paths (PUT,
    /// MKCOL, chunked assembly).  See SPECS/04-tasks/phase-5.md.
    pub(crate) async fn ensure_parent_dir(
        &self,
        fc_path: &str,
    ) -> Result<row::FileCacheRow, String> {
        // Fast path: parent already exists.
        if let Some(row) = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            fc_path,
        )
        .await
        {
            return Ok(row);
        }

        // Build the full chain of ancestors that need to exist.
        // The root "files" must already be in the filecache.
        let segments: Vec<&str> = fc_path.split('/').collect();
        let mut built = String::new();
        let mut last_existing_row: Option<row::FileCacheRow> = None;

        for (i, seg) in segments.iter().enumerate() {
            if i == 0 {
                built.push_str(seg);
            } else {
                built.push('/');
                built.push_str(seg);
            }

            if let Some(r) = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &built,
            )
            .await
            {
                last_existing_row = Some(r);
                continue;
            }

            // Create this missing directory.
            // If even the first segment is missing (e.g. files_trashbin),
            // create it as a peer of "files" — same parent in oc_filecache.
            if last_existing_row.is_none() {
                let files_row = row::lookup_by_path(
                    &self.state.pool,
                    &self.state.table_prefix,
                    self.storage_id,
                    "files",
                )
                .await
                .ok_or("Cannot find root 'files' directory")?;
                // Use files.parent so the new top-level dir is a sibling of
                // "files".  Guard against self-referencing parent.
                let parent_id = if files_row.parent == files_row.fileid {
                    -1
                } else {
                    files_row.parent
                };
                // Synthesise a minimal parent row — only fileid is read by the
                // INSERT below.
                last_existing_row = Some(row::FileCacheRow {
                    fileid: parent_id,
                    storage: self.storage_id,
                    path: None,
                    path_hash: String::new(),
                    parent: -1,
                    name: None,
                    mimetype: 0,
                    mimepart: 0,
                    size: 0,
                    mtime: 0,
                    storage_mtime: 0,
                    etag: None,
                    permissions: 0,
                    checksum: None,
                    creation_time: 0,
                    upload_time: 0,
                });
            }
            let parent_row = last_existing_row.as_ref().unwrap();

            let disk = self.disk_path(&built);
            tokio::fs::create_dir_all(&disk)
                .await
                .map_err(|e| format!("mkdir: {e}"))?;

            let now = now_secs();
            let etag = new_etag();

            // §10.8: mimetype = httpd/unix-directory, mimepart = httpd.
            // Resolved once at startup (phase-21 S3).
            let dir_mime_id = self.state.dir_mime_id;
            let dir_mimepart_id = self.state.dir_mimepart_id;
            let hash = row::path_hash(&built);
            let name = seg.to_string();

            let sql = format!(
                "INSERT INTO {prefix}filecache \
                 (storage, path, path_hash, parent, name, mimetype, mimepart, \
                  size, mtime, storage_mtime, etag, permissions, checksum) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
                 RETURNING fileid",
                prefix = self.state.table_prefix
            );
            let fetched: Result<i64, sqlx::Error> = db_dispatch!(&self.state.pool, |Db, c| {
                sqlx::query_scalar::<Db, _>(&sql)
                    .bind(self.storage_id)
                    .bind(&built)
                    .bind(&hash)
                    .bind(parent_row.fileid)
                    .bind(&name)
                    .bind(dir_mime_id)
                    .bind(dir_mimepart_id)
                    .bind(0i64)
                    .bind(now)
                    .bind(now)
                    .bind(&etag)
                    .bind(31i32)
                    .bind("")
                    .fetch_one(c)
                    .await
            });
            let fid: i64 = match fetched {
                Ok(id) => id,
                Err(e) => {
                    // TOCTOU race: another request created this directory between
                    // our lookup and INSERT.  Re-read the row that the other
                    // request inserted.
                    warn!("ensure_parent_dir insert race for {built}: {e}");
                    if let Some(r) = row::lookup_by_path(
                        &self.state.pool,
                        &self.state.table_prefix,
                        self.storage_id,
                        &built,
                    )
                    .await
                    {
                        last_existing_row = Some(r);
                        continue;
                    }
                    return Err(format!("insert: {e}"));
                }
            };

            // No oc_filecache_extended row is created here — PHP's mkdir path
            // (`View::mkdir` → `Cache::insert`) never writes extension fields
            // (`normalizeData` drops them when absent), so dirs have no extended
            // row.  Creating one with `creation_time = upload_time = now` was a
            // diff-visible divergence on the trashbin ancestor dirs (finding #9).

            let new_row = row::FileCacheRow {
                fileid: fid,
                storage: self.storage_id,
                path: Some(built.clone()),
                path_hash: hash,
                parent: parent_row.fileid,
                name: Some(name),
                mimetype: dir_mime_id,
                mimepart: dir_mimepart_id,
                size: 0,
                mtime: now,
                storage_mtime: now,
                etag: Some(etag),
                permissions: 31,
                checksum: None,
                creation_time: now,
                upload_time: now,
            };

            // PHP `View::mkdir` → `basicOperation` → `Updater::update()`
            // (View.php:1253-1259, Updater.php:68-93) side effects, in the same
            // order: `correctParentStorageMtime(parent)` then `propagateChange`.
            // - The new dir's parent gets `storage_mtime` (and `mtime`, via
            //   Cache::normalizeData's copy magic) from the parent's disk
            //   mtime — so creating `files_trashbin` (a direct child of the
            //   storage root) stamps the root's `storage_mtime` (live-verified
            //   oracle behavior, finding #12).
            // - Every ancestor of the new dir gets one shared etag + bumped
            //   mtime. These stamps are transient for the trash flow (later
            //   propagations overwrite them) but are the mechanism behind the
            //   oracle's `files_trashbin == files_trashbin/files` etag, and
            //   PHP does the same for MKCOL/PUT ancestor mkdirs.
            let parent_fc_path = parent_fc_path(&built);
            let parent_disk = self.disk_path(&parent_fc_path);
            if let Err(e) = self
                .propagator
                .correct_parent_storage_mtime(&parent_fc_path, &parent_disk)
                .await
            {
                tracing::warn!(
                    dir = %built,
                    error = %e,
                    "ensure_parent_dir: parent storage_mtime correction failed"
                );
            }
            if let Err(e) = self.propagator.propagate_change(&built, now, 0).await {
                tracing::warn!(dir = %built, error = %e, "ensure_parent_dir: mkdir propagation failed");
            }

            last_existing_row = Some(new_row);
        }

        last_existing_row.ok_or_else(|| "Failed to ensure parent directory".to_string())
    }

    /// MKCOL.
    pub(crate) async fn create_dir_row(
        &self,
        path: &dav_server::davpath::DavPath,
    ) -> Result<(), FsError> {
        let fc_path = self.to_fc_path(path);
        let disk = self.disk_path(&fc_path);

        // Must not already exist.
        if row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            &fc_path,
        )
        .await
        .is_some()
        {
            return Err(FsError::Exists);
        }

        // Look up parent (auto-create if missing, matching PHP).
        let parent_path = parent_fc_path(&fc_path);
        let parent_row = self
            .ensure_parent_dir(&parent_path)
            .await
            .map_err(|_| FsError::NotFound)?;

        // Create directory on disk.
        blocking(move || std::fs::create_dir(&disk))
            .await
            .map_err(io_to_fs)?;

        // Insert into oc_filecache.
        let now = now_secs();
        let etag = new_etag();

        // §10.8: mimetype = httpd/unix-directory, mimepart = httpd —
        // hoisted onto the AppState ids (PHASE-22 T8.3).
        let dir_mime_id = self.state.dir_mime_id;
        let dir_mimepart_id = self.state.dir_mimepart_id;
        let hash = row::path_hash(&fc_path);
        let name = fc_path.rsplit('/').next().unwrap_or("").to_string();

        let sql = format!(
            "INSERT INTO {prefix}filecache \
             (storage, path, path_hash, parent, name, mimetype, mimepart, \
              size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
             RETURNING fileid",
            prefix = self.state.table_prefix
        );
        let _fid: i64 = db_dispatch!(&self.state.pool, |Db, c| {
            sqlx::query_scalar::<Db, _>(&sql)
                .bind(self.storage_id)
                .bind(&fc_path)
                .bind(&hash)
                .bind(parent_row.fileid)
                .bind(&name)
                .bind(dir_mime_id)
                .bind(dir_mimepart_id)
                .bind(0i64)
                .bind(now)
                .bind(now)
                .bind(&etag)
                .bind(31i32)
                .bind("")
                .fetch_one(c)
                .await
                .map_err(|e| {
                    warn!("create_dir DB insert failed: {e}");
                    FsError::GeneralFailure
                })?
        });

        // §9.2: MKCOL propagates etag/mtime to the parent chain.
        // New directories have size 0, so sizeDifference=0.
        if let Err(e) = self.propagator.propagate_change(&fc_path, now, 0).await {
            tracing::warn!(path = %fc_path, error = %e, "mkcol: propagation failed");
        }

        Ok(())
    }

    /// MOVE.
    pub(crate) async fn rename_node(
        &self,
        from: &dav_server::davpath::DavPath,
        to: &dav_server::davpath::DavPath,
    ) -> Result<(), FsError> {
        let from_fc = self.to_fc_path(from);
        let to_fc = self.to_fc_path(to);

        let from_row = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            &from_fc,
        )
        .await
        .ok_or(FsError::NotFound)?;

        // Resolve new parent.
        let to_parent_fc = parent_fc_path(&to_fc);
        let to_parent = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            &to_parent_fc,
        )
        .await
        .ok_or(FsError::NotFound)?;

        // Move on disk.
        let from_disk = self.disk_path(&from_fc);
        let to_disk = self.disk_path(&to_fc);
        blocking(move || std::fs::rename(&from_disk, &to_disk))
            .await
            .map_err(io_to_fs)?;

        let now = now_secs();
        let new_name = to_fc.rsplit('/').next().unwrap_or("").to_string();
        let new_hash = row::path_hash(&to_fc);
        let prefix = &self.state.table_prefix;

        // Resolve directory mimetype ID early — used both for the
        // directory-guard in the mimetype recomputation below and the
        // subtree-rename check further down.  Hoisted (PHASE-22 T8.3).
        let dir_mime_id = self.state.dir_mime_id;

        // Update the node itself.  Path/path_hash/name/parent only — PHP
        // `Cache::move` (Cache.php:813-831) leaves the moved file's
        // mtime/etag/storage_mtime untouched (live-verified); the
        // parents' etags are bumped by the propagation below.
        let sql_node = format!(
            "UPDATE {prefix}filecache \
             SET path=$1, path_hash=$2, name=$3, parent=$4 \
             WHERE fileid=$5"
        );
        db_dispatch!(&self.state.pool, |Db, c| {
            sqlx::query::<Db>(&sql_node)
                .bind(&to_fc)
                .bind(&new_hash)
                .bind(&new_name)
                .bind(to_parent.fileid)
                .bind(from_row.fileid)
                .execute(c)
                .await
                .map(|_| ())
                .map_err(|_| FsError::GeneralFailure)?
        });

        // §10.10: recompute mimetype + mimepart on extension change.
        // Matches PHP Updater::copyOrRenameFromStorage() — when the
        // source and target extensions differ, the target is not a
        // directory, and the target is not a trashbin file, update
        // the mimetype from the new name.
        let source_name = from_row.name.as_deref().unwrap_or("");
        let source_ext = extension(source_name);
        let target_ext = extension(&new_name);
        let is_dir = from_row.mimetype == dir_mime_id;
        if source_ext != target_ext && !is_dir && !is_trash_extension(target_ext) {
            let new_mime = mime_guess::from_ext(target_ext)
                .first_raw()
                .unwrap_or("application/octet-stream")
                .to_string();
            let new_part = new_mime
                .split('/')
                .next()
                .unwrap_or("application")
                .to_string();
            let new_mid = nc_db::mime::get_or_insert_mime_id(
                &self.state.pool,
                &self.state.table_prefix,
                &self.state.mime_cache,
                &new_mime,
            )
            .await;
            let new_pid = nc_db::mime::get_or_insert_mime_id(
                &self.state.pool,
                &self.state.table_prefix,
                &self.state.mime_cache,
                &new_part,
            )
            .await;
            let sql_mime =
                format!("UPDATE {prefix}filecache SET mimetype=$1, mimepart=$2 WHERE fileid=$3");
            let result = db_execute!(
                &self.state.pool,
                &sql_mime,
                new_mid,
                new_pid,
                from_row.fileid
            );
            if let Err(e) = result {
                tracing::warn!(fileid = from_row.fileid, error = %e, "Failed to update mimetype on rename");
            }
        }

        // Update custom DAV property paths for the renamed node (task §10.11).
        if let Err(e) = crate::row::update_custom_properties_path(
            &self.state.pool,
            prefix,
            &self.uid,
            &from_fc,
            &to_fc,
        )
        .await
        {
            tracing::warn!(from_fc = from_fc, to_fc = to_fc, error = %e, "Failed to update custom property paths on rename");
        }

        // Update all descendants (directory move).
        if from_row.mimetype == dir_mime_id {
            // Bulk-rename all paths under the old prefix using a Rust-side
            // loop (avoids relying on DB-side MD5 dialect differences).
            self.rename_subtree_paths(&from_fc, &to_fc, from_row.fileid, prefix)
                .await;

            // Update custom DAV property paths for the directory subtree
            // (task §10.11).
            crate::row::update_custom_properties_path_subtree(
                &self.state.pool,
                prefix,
                &self.uid,
                self.storage_id,
                &from_fc,
                &to_fc,
            )
            .await;
        }

        // §9.4: relocate versions alongside the renamed file.
        crate::versions::rename_versions(
            &self.state.pool,
            &self.state.table_prefix,
            &self.state.data_directory,
            &self.uid,
            self.storage_id,
            &from_fc,
            &to_fc,
        )
        .await;

        // §9.2: MOVE propagates both source and target chains with
        // sizeDifference=0 (etag/mtime only).  The immediate source/target
        // parents' sizes are fixed by correctFolderSize (PHP
        // Updater.php:195-204).
        let from_parent_fc = parent_fc_path(&from_fc);
        // PHP `copyOrRenameFromStorage` corrects both direct parents'
        // `storage_mtime` from their disk mtimes (Updater.php:198-201).
        for (parent_fc, label) in [
            (from_parent_fc.clone(), "from"),
            (to_parent_fc.clone(), "to"),
        ] {
            let parent_disk = self.disk_path(&parent_fc);
            if let Err(e) = self
                .propagator
                .correct_parent_storage_mtime(&parent_fc, &parent_disk)
                .await
            {
                tracing::warn!(
                    parent = %parent_fc,
                    label,
                    error = %e,
                    "move: parent storage_mtime correction failed"
                );
            }
        }
        if let Err(e) = self.propagator.propagate_change(&from_fc, now, 0).await {
            tracing::warn!(path = %from_fc, error = %e, "move: source propagation failed");
        }
        if let Err(e) = self.propagator.propagate_change(&to_fc, now, 0).await {
            tracing::warn!(path = %to_fc, error = %e, "move: target propagation failed");
        }
        // Recalculate ancestor sizes (size change can't be expressed as a
        // simple signed delta when subtrees move).  Size-ONLY — PHP's
        // `correctFolderSize` never touches etag/mtime, and the standalone
        // `correct_folder_size`'s internal propagation would re-stamp the
        // root after the target-chain etag, breaking `root == files`.
        if from_parent_fc != to_parent_fc {
            if let Err(e) = self
                .propagator
                .correct_folder_size_chain(&from_parent_fc)
                .await
            {
                tracing::warn!(path = %from_parent_fc, error = %e, "move: correct_folder_size_chain from failed");
            }
            if let Err(e) = self
                .propagator
                .correct_folder_size_chain(&to_parent_fc)
                .await
            {
                tracing::warn!(path = %to_parent_fc, error = %e, "move: correct_folder_size_chain to failed");
            }
        } else {
            // Same parent (rename within the same directory) — recalculate once.
            if let Err(e) = self
                .propagator
                .correct_folder_size_chain(&to_parent_fc)
                .await
            {
                tracing::warn!(path = %to_parent_fc, error = %e, "move: correct_folder_size_chain failed");
            }
        }

        Ok(())
    }

    /// COPY.
    pub(crate) async fn copy_node(
        &self,
        from: &dav_server::davpath::DavPath,
        to: &dav_server::davpath::DavPath,
    ) -> Result<(), FsError> {
        let from_fc = self.to_fc_path(from);
        let to_fc = self.to_fc_path(to);
        let from_disk = self.disk_path(&from_fc);
        let to_disk = self.disk_path(&to_fc);

        blocking(move || std::fs::copy(&from_disk, &to_disk).map(|_| ()))
            .await
            .map_err(io_to_fs)?;

        // For simplicity: remove old DB row for destination if exists,
        // then insert new row by re-reading disk metadata.
        let copy_del_sql = format!(
            "DELETE FROM {prefix}filecache \
             WHERE storage = $1 AND path_hash = $2",
            prefix = self.state.table_prefix
        );
        let result = db_execute!(
            &self.state.pool,
            &copy_del_sql,
            self.storage_id,
            row::path_hash(&to_fc)
        );
        if let Err(e) = result {
            tracing::warn!(to_fc = to_fc, error = %e, "Failed to delete old filecache row on copy");
        }

        if let Some(from_row) = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            &from_fc,
        )
        .await
        {
            let to_parent_fc = parent_fc_path(&to_fc);
            // PHP's copy scans the target parent into the cache when it's
            // missing (Updater.php:141-148 — the disk copy needs the dir
            // too).  A COPY into a fresh subdir must create it
            // (live-verified: the oracle answers 201 with the dir
            // created; the SUT previously returned 409).
            self.ensure_parent_dir(&to_parent_fc)
                .await
                .map_err(|_| FsError::NotFound)?;
            if let Some(parent_row) = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &to_parent_fc,
            )
            .await
            {
                let now = now_secs();
                // The copied row is a CLONE of the source (PHP
                // `copyFromCache`): it inherits the source's etag and
                // mtime, drops the checksum (NULL), and takes
                // `storage_mtime` = the copy time (the copied file's disk
                // mtime — PHP `updateStorageMTimeOnly`).
                let etag = from_row.etag.clone().unwrap_or_else(|| new_etag());
                let (src_creation, src_upload) = {
                    let ext = row::get_extended(
                        &self.state.pool,
                        &self.state.table_prefix,
                        from_row.fileid,
                    )
                    .await;
                    (ext.creation_time, ext.upload_time)
                };
                let name = to_fc.rsplit('/').next().unwrap_or("").to_string();
                let hash = row::path_hash(&to_fc);
                let prefix = &self.state.table_prefix;

                // §10.10: recompute mimetype on extension change for COPY.
                // Matches PHP Updater::copyOrRenameFromStorage() —
                // skip for directories and trash targets.  Hoisted
                // (PHASE-22 T8.3).
                let dir_mime_id = self.state.dir_mime_id;
                let source_name = from_row.name.as_deref().unwrap_or("");
                let source_ext = extension(source_name);
                let target_ext = extension(&name);
                let is_dir = from_row.mimetype == dir_mime_id;
                let (copy_mid, copy_pid) =
                    if source_ext != target_ext && !is_dir && !is_trash_extension(target_ext) {
                        let new_mime = mime_guess::from_ext(target_ext)
                            .first_raw()
                            .unwrap_or("application/octet-stream")
                            .to_string();
                        let new_part = new_mime
                            .split('/')
                            .next()
                            .unwrap_or("application")
                            .to_string();
                        let mid = nc_db::mime::get_or_insert_mime_id(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &self.state.mime_cache,
                            &new_mime,
                        )
                        .await;
                        let pid = nc_db::mime::get_or_insert_mime_id(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &self.state.mime_cache,
                            &new_part,
                        )
                        .await;
                        (mid, pid)
                    } else {
                        (from_row.mimetype, from_row.mimepart)
                    };

                // `checksum` intentionally unbound: the clone drops it
                // (NULL), matching the oracle.
                let sql = format!(
                    "INSERT INTO {prefix}filecache \
                     (storage, path, path_hash, parent, name, mimetype, mimepart, \
                      size, mtime, storage_mtime, etag, permissions) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
                     RETURNING fileid"
                );
                let copy_fetched: Result<i64, sqlx::Error> =
                    db_dispatch!(&self.state.pool, |Db, c| {
                        sqlx::query_scalar::<Db, _>(&sql)
                            .bind(self.storage_id)
                            .bind(&to_fc)
                            .bind(&hash)
                            .bind(parent_row.fileid)
                            .bind(&name)
                            .bind(copy_mid)
                            .bind(copy_pid)
                            .bind(from_row.size)
                            .bind(from_row.mtime)
                            .bind(now)
                            .bind(&etag)
                            .bind(from_row.permissions)
                            .fetch_one(c)
                            .await
                    });
                let copy_fid: Option<i64> = match copy_fetched {
                    Ok(fid) => Some(fid),
                    Err(e) => {
                        tracing::warn!(to_fc = to_fc, error = %e, "Failed to insert filecache row on copy");
                        None
                    }
                };

                if let Some(fid) = copy_fid {
                    // The copied row inherits the source's extended row
                    // (creation_time/upload_time) — the oracle's copies
                    // carry one.
                    let sql_ext = format!(
                        "INSERT INTO {prefix}filecache_extended \
                         (fileid, metadata_etag, creation_time, upload_time) \
                         VALUES ($1, '', $2, $3) \
                         ON CONFLICT(fileid) DO NOTHING",
                        prefix = self.state.table_prefix
                    );
                    let result =
                        db_execute!(&self.state.pool, &sql_ext, fid, src_creation, src_upload);
                    if let Err(e) = result {
                        tracing::warn!(fileid = fid, error = %e, "Failed to insert copy extended row");
                    }

                    // §9.4: insert oc_files_versions for the copied file.
                    // Matches PHP NodeCreatedEvent → created() → createVersionEntity().
                    crate::versions::insert_version_entity(
                        &self.state.pool,
                        &self.state.table_prefix,
                        fid,
                        now,
                        from_row.size,
                        copy_mid,
                        &self.uid,
                    )
                    .await;

                    // PHP's copy dispatches NodeWrittenEvent → the
                    // previewgenerator PostWriteListener queues a
                    // preview_generation row for the copy.
                    crate::preview_queue::queue_preview_generation(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.uid,
                        fid,
                        now,
                    )
                    .await;
                }
            }
        }

        // §9.4: copy versions alongside the copied file.
        crate::versions::copy_versions(
            &self.state.pool,
            &self.state.table_prefix,
            &self.state.data_directory,
            &self.uid,
            self.storage_id,
            &from_fc,
            &to_fc,
        )
        .await;

        // §9.2: COPY propagates the target chain with
        // sizeDifference=0 (etag/mtime only), then corrects the
        // immediate target parent size (PHP Updater.php:195-204).
        let now = now_secs();
        if let Err(e) = self.propagator.propagate_change(&to_fc, now, 0).await {
            tracing::warn!(path = %to_fc, error = %e, "copy: propagation failed");
        }
        // Size-ONLY (PHP correctFolderSize never touches etag/mtime — the
        // standalone correct_folder_size would re-stamp the root after
        // the target-chain etag and break `root == files`).  Chained on
        // the TARGET so the copy's own parent (e.g. a freshly-created
        // subdir) gets its size recomputed from the new child
        // (live-verified: the oracle's copy-dir carries the copy's size).
        if let Err(e) = self.propagator.correct_folder_size_chain(&to_fc).await {
            tracing::warn!(path = %to_fc, error = %e, "copy: correct_folder_size_chain failed");
        }

        Ok(())
    }

    /// The mtime-changing PROPPATCH path.
    pub(crate) async fn set_mtime(
        &self,
        path: &dav_server::davpath::DavPath,
        tm: std::time::SystemTime,
    ) -> Result<(), FsError> {
        let fc_path = self.to_fc_path(path);
        let mtime = tm
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let sql = format!(
            "UPDATE {prefix}filecache SET mtime=$1, storage_mtime=$2 WHERE storage=$3 AND path_hash=$4",
            prefix = self.state.table_prefix
        );
        db_dispatch!(&self.state.pool, |Db, c| {
            sqlx::query::<Db>(&sql)
                .bind(mtime)
                .bind(mtime)
                .bind(self.storage_id)
                .bind(row::path_hash(&fc_path))
                .execute(c)
                .await
                .map(|_| ())
                .map_err(|_| FsError::GeneralFailure)?
        });

        // §9.2: mtime-changing PROPPATCH propagates etag/mtime to
        // ancestors (sizeDifference=0).
        if let Err(e) = self.propagator.propagate_change(&fc_path, mtime, 0).await {
            tracing::warn!(path = %fc_path, error = %e, "set_modified: propagation failed");
        }

        Ok(())
    }

    /// Rename all oc_filecache paths below `old_prefix` to `new_prefix`.
    ///
    /// This fetches every matching row and re-inserts the path_hash in Rust
    /// to avoid relying on a DB-side MD5 function that may not exist in SQLite.
    async fn rename_subtree_paths(
        &self,
        old_prefix: &str,
        new_prefix: &str,
        _dir_fileid: i64,
        prefix: &str,
    ) {
        let like = format!("{old_prefix}/%");
        let sql_fetch = format!(
            "SELECT fileid, path FROM {prefix}filecache WHERE storage = $1 AND path LIKE $2"
        );
        let rows: Vec<(i64, String)> = db_dispatch!(&self.state.pool, |Db, c| {
            sqlx::query::<Db>(&sql_fetch)
                .bind(self.storage_id)
                .bind(&like)
                .fetch_all(c)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.get::<i64, _>("fileid"), r.get::<String, _>("path")))
                .collect()
        });

        for (fileid, old_path) in rows {
            let new_path = format!("{new_prefix}{}", &old_path[old_prefix.len()..]);
            let new_hash = row::path_hash(&new_path);
            let sql_upd =
                format!("UPDATE {prefix}filecache SET path=$1, path_hash=$2 WHERE fileid=$3");
            let _ = db_execute!(&self.state.pool, &sql_upd, &new_path, &new_hash, fileid);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::path_utils::parent_fc_path;
    use crate::row;
    use crate::testing::{extended_count, fresh_data_dir, fresh_delete_db, test_fs};

    // ── Upload path computation ────────────────────────────────────────────
    //
    // These tests verify path construction used by all write paths,
    // matching:
    //   lib/private/Files/View.php::createParentDirectories()
    //     → recursive parent directory auto-creation
    //   apps/files_trashbin/lib/Trashbin.php::setUpTrash()
    //     → creates files_trashbin/files as sibling of files/
    //   core-rs/crates/nc-dav/src/row.rs::dav_to_fc_path()
    //     → DAV path to filecache path mapping

    /// Simulate `dav_to_fc_path` — the DAV→filecache path mapping used by
    /// all write paths (PUT, MKCOL, bulk upload).
    fn upload_fc_path(dav_path: &str) -> String {
        crate::row::dav_to_fc_path(dav_path)
    }

    /// Build the chain of ancestor paths from a filecache path, matching
    /// the segment-iteration logic in `ensure_parent_dir`.
    fn upload_ancestor_chain(fc_path: &str) -> Vec<String> {
        let segments: Vec<&str> = fc_path.split('/').collect();
        let mut built = String::new();
        let mut chain = Vec::new();
        for (i, seg) in segments.iter().enumerate() {
            if i == 0 {
                built.push_str(seg);
            } else {
                built.push('/');
                built.push_str(seg);
            }
            chain.push(built.clone());
        }
        chain
    }

    #[test]
    fn upload_fc_path_root() {
        assert_eq!(upload_fc_path("/"), "files");
    }

    #[test]
    fn upload_fc_path_root_level_file() {
        assert_eq!(upload_fc_path("/test.txt"), "files/test.txt");
    }

    #[test]
    fn upload_fc_path_nested_file() {
        assert_eq!(
            upload_fc_path("/Media/Decent photos/001.jpg"),
            "files/Media/Decent photos/001.jpg"
        );
    }

    #[test]
    fn upload_fc_path_no_leading_slash() {
        // DAV paths may or may not have a leading slash.
        assert_eq!(upload_fc_path("Media/test.txt"), "files/Media/test.txt");
    }

    #[test]
    fn upload_fc_path_no_double_files_prefix() {
        // Regression: bulk_handler used to prepend "files/" to a path
        // that already contained "files/", producing "files/files/...".
        // dav_to_fc_path trims slashes and adds "files/", so a raw "files/foo"
        // would produce "files/files/foo". Callers must strip "files/" first.
        //
        // This test documents the CONTRACT: dav_to_fc_path always adds
        // "files/", so callers must NOT pass a path already containing it.
        let with_leading_files = upload_fc_path("files/test.txt");
        assert_eq!(with_leading_files, "files/files/test.txt");
        // The correct usage: strip the DAV prefix first, then convert.
        assert_eq!(upload_fc_path("test.txt"), "files/test.txt");
    }

    #[test]
    fn upload_parent_root_level() {
        // Parent of "files/test.txt" is "files".
        assert_eq!(parent_fc_path("files/test.txt"), "files");
    }

    #[test]
    fn upload_parent_nested() {
        assert_eq!(
            parent_fc_path("files/Media/Decent photos/001.jpg"),
            "files/Media/Decent photos"
        );
    }

    #[test]
    fn upload_parent_two_levels() {
        assert_eq!(parent_fc_path("files/a/b"), "files/a");
    }

    #[test]
    fn upload_ancestor_chain_root_level() {
        let chain = upload_ancestor_chain("files/test.txt");
        assert_eq!(chain, vec!["files", "files/test.txt"]);
    }

    #[test]
    fn upload_ancestor_chain_nested_directory() {
        // Uploading to "files/a/b/c" should create ancestors:
        // "files", "files/a", "files/a/b", "files/a/b/c".
        let chain = upload_ancestor_chain("files/a/b/c");
        assert_eq!(chain, vec!["files", "files/a", "files/a/b", "files/a/b/c",]);
    }

    #[test]
    fn upload_ancestor_chain_trashbin() {
        // files_trashbin is a peer of files — its first segment
        // triggers the "if last_existing_row.is_none()" branch in
        // ensure_parent_dir.
        let chain = upload_ancestor_chain("files_trashbin/files/test.d123");
        assert_eq!(
            chain,
            vec![
                "files_trashbin",
                "files_trashbin/files",
                "files_trashbin/files/test.d123",
            ]
        );
    }

    #[test]
    fn upload_ancestor_chain_deep_trashbin() {
        let chain = upload_ancestor_chain("files_trashbin/files/foo.d123/sub.txt");
        assert_eq!(
            chain,
            vec![
                "files_trashbin",
                "files_trashbin/files",
                "files_trashbin/files/foo.d123",
                "files_trashbin/files/foo.d123/sub.txt",
            ]
        );
    }

    /// Finding #9: ensure_parent_dir creates filecache dirs WITHOUT
    /// oc_filecache_extended rows (PHP `View::mkdir` → `Cache::insert` writes
    /// no extension fields for directories).
    #[tokio::test]
    async fn ensure_parent_dir_creates_no_extended_rows() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir);

        fs.ensure_parent_dir("files_trashbin/files/x")
            .await
            .unwrap();
        for p in [
            "files_trashbin",
            "files_trashbin/files",
            "files_trashbin/files/x",
        ] {
            assert!(
                row::lookup_by_path(&pool, &prefix, storage_id, p)
                    .await
                    .is_some(),
                "missing dir {p}"
            );
        }
        assert_eq!(
            extended_count(&pool).await,
            0,
            "no extended rows for mkdir'd dirs"
        );
    }
}
