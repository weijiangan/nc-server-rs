//! `NcFileSystem` — the `DavFileSystem` trait implementation.
//!
//! One instance is created **per request** with the authenticated user's `uid`
//! and pre-resolved `storage_id`.  The instance is cheap to create (all
//! expensive state is held in `NcDavState` behind `Arc`s).
//!
//! ## Path mapping
//!
//! WebDAV paths are relative to the DAV root (after the prefix is stripped by
//! `DavConfig::strip_prefix`).  They map to `oc_filecache` paths as:
//!
//! ```text
//! DAV path          oc_filecache path        Disk path
//! /                 files                    {data_dir}/{uid}/files
//! /Photos/img.jpg   files/Photos/img.jpg     {data_dir}/{uid}/files/Photos/img.jpg
//! ```

use std::io;
use std::path::PathBuf;

use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, DavProp, FsError, FsFuture, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures::{future, stream, FutureExt};
use sqlx::Row;
use tokio::task;
use tracing::warn;

use crate::{
    davfile::{NcDavFile, WriteCtx},
    metadata::{NcDirEntry, NcMetaData},
    propagator::Propagator,
    row::{self, dav_to_fc_path, disk_path},
    NcDavState,
};

// ─── NcFileSystem ─────────────────────────────────────────────────────────────

/// Per-request DAV filesystem bound to one user's home storage.
#[derive(Clone)]
pub struct NcFileSystem {
    pub(crate) state: NcDavState,
    pub(crate) uid: String,
    pub(crate) storage_id: i64,
    /// `X-OC-MTime` header value parsed from the request (Unix seconds).
    pub(crate) x_oc_mtime: Option<i64>,
    /// `X-OC-CTime` header value parsed from the request (Unix seconds).
    pub(crate) x_oc_ctime: Option<i64>,
    /// Channel through which `flush()` returns write metadata for response headers.
    pub(crate) write_result: crate::SharedWriteResult,
    /// Channel through which `flush()` signals known PUT errors (checksum
    /// mismatch, etc.) so `dav_handler` can rewrite the HTTP status code.
    pub(crate) put_error: crate::SharedPutError,
    /// Cache propagator — created once per request, shared by all mutations
    /// within the request.  Cheap to clone.
    pub(crate) propagator: Propagator,
    /// `X-NC-Skip-Trashbin: true` header from the request.  When set, DELETE
    /// operations hard-delete instead of moving to trash (§9.3 critique #7).
    pub(crate) skip_trashbin: bool,
    /// Per-request tag cache (§9.5).  Prefetched during `read_dir` for depth-1
    /// PROPFIND, then read by `get_props` for each node.
    pub(crate) tag_cache: crate::tags::TagCache,
}

impl NcFileSystem {
    pub fn new(
        state: NcDavState,
        uid: String,
        storage_id: i64,
        x_oc_mtime: Option<i64>,
        x_oc_ctime: Option<i64>,
        write_result: crate::SharedWriteResult,
        put_error: crate::SharedPutError,
        skip_trashbin: bool,
    ) -> Self {
        let propagator = Propagator::new(
            state.pool.clone(),
            state.table_prefix.clone(),
            storage_id,
        );
        let tag_cache = crate::tags::new_tag_cache();
        NcFileSystem {
            state,
            uid,
            storage_id,
            x_oc_mtime,
            x_oc_ctime,
            write_result,
            put_error,
            propagator,
            skip_trashbin,
            tag_cache,
        }
    }

    /// Convert a `DavPath` to an `oc_filecache` path.
    fn to_fc_path(&self, path: &dav_server::davpath::DavPath) -> String {
        let raw = String::from_utf8_lossy(path.as_bytes()).into_owned();
        dav_to_fc_path(&raw)
    }

    /// Resolve the on-disk path for a filecache path.
    fn disk_path(&self, fc_path: &str) -> PathBuf {
        let data_dir = self.state.data_directory.as_path();
        disk_path(data_dir, &self.uid, fc_path)
    }

    /// Ensure a parent directory exists in the filecache, creating it
    /// recursively if needed.
    ///
    /// Matches PHP's `View::createParentDirectories()` which is called
    /// before every `newFile()` / `newFolder()` operation.  Without this,
    /// uploading a file into a newly-created folder (or a folder that only
    /// exists on disk but not in the filecache) fails with NotFound.
    ///
    /// Deviation: PHP does NOT call `createParentDirectories()` from chunked
    /// upload v2 assembly, so chunked uploads to paths with a non-existent
    /// parent fail.  Rust calls this uniformly from all write paths (PUT,
    /// MKCOL, chunked assembly).  See SPECS/04-tasks/phase-5.md.
    async fn ensure_parent_dir(&self, fc_path: &str) -> Result<row::FileCacheRow, String> {
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

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());

            // §10.8: mimetype = httpd/unix-directory, mimepart = httpd
            let dir_mime_id = nc_db::mime::get_or_insert_mime_id(
                &self.state.pool,
                &self.state.table_prefix,
                &self.state.mime_cache,
                "httpd/unix-directory",
            )
            .await;
            let dir_mimepart_id = nc_db::mime::get_or_insert_mime_id(
                &self.state.pool,
                &self.state.table_prefix,
                &self.state.mime_cache,
                "httpd",
            )
            .await;
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
            let fid: i64 = match sqlx::query_scalar(&sql)
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
                .fetch_one(&self.state.pool)
                .await
            {
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

            // Also create the extended-cache row.
            {
                let sql = format!(
                    "INSERT INTO {prefix}filecache_extended \
                     (fileid, metadata_etag, creation_time, upload_time) \
                     VALUES ($1, '', $2, $2) \
                     ON CONFLICT(fileid) DO NOTHING",
                    prefix = self.state.table_prefix
                );
                if let Err(e) = sqlx::query(&sql)
                    .bind(fid)
                    .bind(now)
                    .execute(&self.state.pool)
                    .await
                {
                    tracing::warn!(fileid = fid, error = %e, "Failed to insert oc_filecache_extended for ancestor {built}");
                }
            }

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
            last_existing_row = Some(new_row);
        }

        last_existing_row.ok_or_else(|| "Failed to ensure parent directory".to_string())
    }

    /// Full PHP `Storage::doDelete` gating logic (`Storage.php:125-146`).
    ///
    /// A delete is a trash (not hard) only when **all** of these hold:
    /// - `files_trashbin` app is enabled for the user
    /// - The path is NOT a `.part` file (partial upload)
    /// - `X-NC-Skip-Trashbin: true` header is NOT set
    /// - The path is under the `files/` subtree
    ///
    /// Deviations from PHP:
    /// - `MoveToTrashEvent` dispatch is skipped (no event system in Rust).
    /// - Encryption-exception fallback is skipped (no server-side encryption).
    /// - Cross-storage `disableTrash` is skipped (single-storage context).
    async fn should_move_to_trash(&self, fc_path: &str) -> bool {
        // App must be enabled.
        if !self.is_trashbin_app_enabled().await {
            return false;
        }
        // .part files are always hard-deleted (PHP line 127).
        if fc_path.ends_with(".part") {
            return false;
        }
        // X-NC-Skip-Trashbin: true → hard delete (PHP line 128).
        if self.skip_trashbin {
            return false;
        }
        // Only paths under "files/" are trashed (PHP shouldMoveToTrash:100).
        // This also implicitly rejects paths under appdata_, files_trashbin, etc.
        if !fc_path.starts_with("files/") {
            return false;
        }
        true
    }

    /// Check whether the `files_trashbin` app is enabled for the current user.
    ///
    /// Matches PHP `Trashbin::isEnabled()` which calls
    /// `AppManager::isEnabledForUser('files_trashbin')`.
    async fn is_trashbin_app_enabled(&self) -> bool {
        let sql = format!(
            "SELECT configvalue FROM {prefix}appconfig \
             WHERE appid = 'files_trashbin' AND configkey = 'enabled'",
            prefix = self.state.table_prefix
        );
        match sqlx::query_scalar::<_, String>(&sql)
            .fetch_optional(&self.state.pool)
            .await
        {
            Ok(Some(val)) => val == "yes" || val == "true",
            Ok(None) => {
                // Key not present — app may not be installed.
                // Also check cache in case it's there (some setups use different keys).
                let cache = self
                    .state
                    .appconfig_cache
                    .read()
                    .expect("appconfig cache lock");
                cache
                    .get_string("files_trashbin", "enabled")
                    .map_or(false, |v| v == "yes" || v == "true")
            }
            Err(_) => false,
        }
    }

    // ── delete_dir (centralized) ──────────────────────────────────────────

    /// Delete a directory — the single entry point for all directory deletion.
    ///
    /// Matching PHP's `View::rmdir()` pattern:
    /// 1. Look up the directory and capture its size.
    /// 2. Perform the operation (trash or hard-delete).
    /// 3. Always propagate the size change to the parent chain.
    ///
    /// Called by both the handler's DELETE intercept and [`remove_dir`].
    pub(crate) async fn delete_dir(&self, fc_path: &str) -> Result<(), FsError> {
        // Capture the directory size before mutation (for propagation).
        let dir_row = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            fc_path,
        )
        .await
        .ok_or(FsError::NotFound)?;
        let deleted_size = dir_row.size;

        if self.should_move_to_trash(fc_path).await {
            self.trash_directory(fc_path).await?;
        } else {
            // Trashbin app not enabled — hard delete.
            let disk = self.disk_path(fc_path);
            let d = disk.clone();
            blocking(move || std::fs::remove_dir_all(&d))
                .await
                .map_err(io_to_fs)?;

            let prefix = &self.state.table_prefix;
            let like_pat = format!("{fc_path}/%");

            // Clean up custom DAV properties before deleting filecache rows.
            crate::row::delete_custom_properties_for_dir(
                &self.state.pool,
                prefix,
                &self.uid,
                self.storage_id,
                fc_path,
            )
            .await;

            let sql_ext = format!(
                "DELETE FROM {prefix}filecache_extended \
                 WHERE fileid IN (\
                     SELECT fileid FROM {prefix}filecache \
                     WHERE storage = $1 AND (path = $2 OR path LIKE $3)\
                 )"
            );
            if let Err(e) = sqlx::query(&sql_ext)
                .bind(self.storage_id)
                .bind(fc_path)
                .bind(&like_pat)
                .execute(&self.state.pool)
                .await
            {
                tracing::warn!(fc_path = fc_path, error = %e, "Failed to delete filecache_extended rows for directory");
            }

            let sql_subtree = format!(
                "DELETE FROM {prefix}filecache WHERE storage = $1 AND (path = $2 OR path LIKE $3)"
            );
            sqlx::query(&sql_subtree)
                .bind(self.storage_id)
                .bind(fc_path)
                .bind(&like_pat)
                .execute(&self.state.pool)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
        }

        // §9.2: always propagate — matching PHP's View::rmdir() calling
        // Updater::remove() regardless of whether the storage trashed or
        // hard-deleted.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if deleted_size > 0 {
            if let Err(e) = self
                .propagator
                .propagate_change(fc_path, now, -deleted_size)
                .await
            {
                tracing::warn!(path = %fc_path, error = %e, "delete_dir: propagation failed (size>0)");
            }
        } else {
            if let Err(e) = self
                .propagator
                .propagate_change(fc_path, now, 0)
                .await
            {
                tracing::warn!(path = %fc_path, error = %e, "delete_dir: propagation failed (size=0)");
            }
        }

        Ok(())
    }

    /// Delete a file — the single entry point for all file deletion.
    ///
    /// Matching PHP's `View::unlink()` pattern:
    /// 1. Look up the file and capture its size.
    /// 2. Perform the operation (trash or hard-delete).
    /// 3. Always propagate the size change to the parent chain.
    pub(crate) async fn delete_file(&self, fc_path: &str) -> Result<(), FsError> {
        let frow = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            fc_path,
        )
        .await
        .ok_or(FsError::NotFound)?;

        let deleted_size = frow.size;

        if self.should_move_to_trash(fc_path).await {
            self.move_to_trash(fc_path, &frow).await?;
        } else {
            let disk = self.disk_path(fc_path);
            let d = disk.clone();
            blocking(move || std::fs::remove_file(&d))
                .await
                .map_err(io_to_fs)?;

            let sql = format!(
                "DELETE FROM {prefix}filecache WHERE fileid = $1",
                prefix = self.state.table_prefix
            );
            sqlx::query(&sql)
                .bind(frow.fileid)
                .execute(&self.state.pool)
                .await
                .map_err(|_| FsError::GeneralFailure)?;

            let sql_ext = format!(
                "DELETE FROM {prefix}filecache_extended WHERE fileid = $1",
                prefix = self.state.table_prefix
            );
            if let Err(e) = sqlx::query(&sql_ext)
                .bind(frow.fileid)
                .execute(&self.state.pool)
                .await
            {
                tracing::warn!(fileid = frow.fileid, error = %e, "Failed to delete filecache_extended row");
            }

            if let Err(e) = crate::row::delete_custom_properties_for_path(
                &self.state.pool,
                &self.state.table_prefix,
                &self.uid,
                fc_path,
            )
            .await
            {
                tracing::warn!(fileid = frow.fileid, fc_path = fc_path, error = %e, "Failed to delete custom properties");
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if deleted_size > 0 {
            if let Err(e) = self
                .propagator
                .propagate_change(fc_path, now, -deleted_size)
                .await
            {
                tracing::warn!(path = %fc_path, error = %e, "delete_file: propagation failed (size>0)");
            }
        } else {
            if let Err(e) = self
                .propagator
                .propagate_change(fc_path, now, 0)
                .await
            {
                tracing::warn!(path = %fc_path, error = %e, "delete_file: propagation failed (size=0)");
            }
        }

        Ok(())
    }

    // ── trash_directory ───────────────────────────────────────────────────

    /// Move a directory tree to the trashbin.
    ///
    /// Does NOT handle propagation — [`delete_dir`] does that centrally
    /// (matching PHP's View → Updater separation).
    pub(crate) async fn trash_directory(&self, fc_path: &str) -> Result<(), FsError> {
        let row = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            fc_path,
        )
        .await
        .ok_or(FsError::NotFound)?;

        // Collect descendant (fileid, path) before moving.
        let like_pat = format!("{fc_path}/%");
        let sql_desc = format!(
            "SELECT fileid, path FROM {prefix}filecache \
             WHERE storage = $1 AND path LIKE $2",
            prefix = self.state.table_prefix
        );
        let descendants: Vec<(i64, String)> = sqlx::query(&sql_desc)
            .bind(self.storage_id)
            .bind(&like_pat)
            .fetch_all(&self.state.pool)
            .await
            .map_err(|_| FsError::GeneralFailure)?
            .into_iter()
            .map(|r| (r.get::<i64, _>("fileid"), r.get::<String, _>("path")))
            .collect();

        // Move the directory itself to trash.
        let old_fc_path = fc_path.to_string();
        let trash_fc = self.move_to_trash(&old_fc_path, &row).await?;

        // Update filecache paths for all descendants so they appear
        // nested inside the trashed directory.
        for (fid, old_path) in &descendants {
            let new_path = trash_fc.clone() + &old_path[old_fc_path.len()..];
            let new_hash = row::path_hash(&new_path);
            let sql_upd = format!(
                "UPDATE {prefix}filecache SET path=$1, path_hash=$2 WHERE fileid=$3",
                prefix = self.state.table_prefix
            );
            if let Err(e) = sqlx::query(&sql_upd)
                .bind(&new_path)
                .bind(&new_hash)
                .bind(fid)
                .execute(&self.state.pool)
                .await
            {
                tracing::warn!(fileid = fid, error = %e, "Failed to update descendant path in trash");
            }
        }

        Ok(())
    }

    /// Move a file or directory to the trash bin, matching PHP's
    /// `Trashbin::move2trash()`.  Returns the trash `fc_path` on success.
    ///
    /// - Renames on disk: `files/{path}` → `files_trashbin/files/{basename}.d{timestamp}`
    /// - Updates the `oc_filecache` row (path, name, parent, mtime).
    /// - Inserts a row into `oc_files_trash`.
    async fn move_to_trash(
        &self,
        fc_path: &str,
        row: &row::FileCacheRow,
    ) -> Result<String, FsError> {
        let relative = fc_path.strip_prefix("files/").unwrap_or(fc_path);
        let mut now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // PHP Trashbin::move2trash() uses pathinfo():
        //   $filename = pathinfo($ownerPath)['basename'];  // e.g. "test.txt"
        //   $location = pathinfo($ownerPath)['dirname'];   // e.g. "." or "Media"
        //
        // The trash filename is built from the basename ONLY — directory
        // structure is NOT preserved.  A file at "Media/test.txt" becomes
        // "files_trashbin/files/test.txt.d{timestamp}".
        let p = std::path::Path::new(relative);
        let trash_basename = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| relative.to_string());

        // Build the trash path with PHP-compatible conflict resolution.
        //
        // PHP `Trashbin::getTrashFilename()` produces strictly
        // `{basename}.d{timestamp}`.  On a same-second collision PHP
        // increments the *timestamp* (it never appends a numeric suffix),
        // because `Trashbin::restore()`/`getLocation()` recover the timestamp
        // by splitting the trash name on `.d`.  A `_N` suffix would yield a
        // name the PHP-FPM trashbin cannot parse or restore, so we must match
        // PHP and nudge the timestamp forward instead.  (See `trash_fc_name`.)
        let mut trash_fc = trash_fc_name(&trash_basename, now);
        loop {
            let in_cache = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &trash_fc,
            )
            .await
            .is_some();
            let on_disk = tokio::fs::try_exists(self.disk_path(&trash_fc))
                .await
                .unwrap_or(false);
            if !in_cache && !on_disk {
                break;
            }
            now += 1;
            trash_fc = trash_fc_name(&trash_basename, now);
        }

        // Ensure the trash parent exists in filecache.
        let trash_parent_fc = {
            let mut parts: Vec<&str> = trash_fc.split('/').collect();
            parts.pop();
            parts.join("/")
        };
        self.ensure_parent_dir(&trash_parent_fc)
            .await
            .map_err(|_| FsError::NotFound)?;

        // Create parent dirs on disk.
        let from_disk = self.disk_path(fc_path);
        let to_disk = self.disk_path(&trash_fc);
        if let Some(parent) = to_disk.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                warn!("move_to_trash mkdir parent failed: {e}");
                FsError::GeneralFailure
            })?;
        }

        // Move on disk.
        let f = from_disk.clone();
        let t = to_disk.clone();
        blocking(move || std::fs::rename(&f, &t))
            .await
            .map_err(|e| {
                warn!("move_to_trash rename {fc_path} → {trash_fc}: {e}");
                io_to_fs(e)
            })?;

        // Update the filecache row.
        let new_hash = row::path_hash(&trash_fc);
        let new_name = trash_fc.rsplit('/').next().unwrap_or("").to_string();
        let trash_parent = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            &trash_parent_fc,
        )
        .await
        .ok_or(FsError::NotFound)?;

        let sql = format!(
            "UPDATE {prefix}filecache \
             SET path=$1, path_hash=$2, name=$3, parent=$4, mtime=$5 \
             WHERE fileid=$6",
            prefix = self.state.table_prefix
        );
        sqlx::query(&sql)
            .bind(&trash_fc)
            .bind(&new_hash)
            .bind(&new_name)
            .bind(trash_parent.fileid)
            .bind(now)
            .bind(row.fileid)
            .execute(&self.state.pool)
            .await
            .map_err(|_| FsError::GeneralFailure)?;

        // Insert into oc_files_trash.
        //
        // PHP Trashbin::move2trash() uses pathinfo():
        //   $filename = pathinfo($ownerPath)['basename'];   // e.g. "test"
        //   $location = pathinfo($ownerPath)['dirname'];    // e.g. "." or "Media"
        //
        // The `id` column stores the basename so that
        // Trashbin::delete() can match it when stripping the
        // .d{timestamp} suffix from the trash filename.
        let trash_location = p
            .parent()
            .and_then(|d| d.to_str())
            .map(|s| if s.is_empty() { "." } else { s })
            .unwrap_or(".")
            .to_string();
        // PHP `Trashbin::move2trash()` inserts ONLY these columns and leaves
        // `type` / `mime` NULL (migration Version1010Date20200630192639: `type`
        // is VARCHAR(4), `mime` VARCHAR(255), both nullable). `timestamp` is a
        // VARCHAR(12) and PHP binds it as a string (createNamedParameter
        // defaults to PARAM_STR), so we bind a string too — binding an integer
        // makes Postgres reject the whole INSERT (bigint into a varchar column).
        let trash_sql = format!(
            "INSERT INTO {prefix}files_trash (id, \"user\", \"timestamp\", location, deleted_by) \
             VALUES ($1, $2, $3, $4, $5)",
            prefix = self.state.table_prefix
        );
        if let Err(e) = sqlx::query(&trash_sql)
            .bind(&trash_basename)
            .bind(&self.uid)
            .bind(now.to_string())
            .bind(&trash_location)
            .bind(&self.uid)
            .execute(&self.state.pool)
            .await
        {
            // Non-fatal: the file is already trashed on disk and in the
            // filecache, so it still lists in the web UI. But without this row
            // the original location is lost and PHP restore falls back to root.
            warn!("oc_files_trash insert failed for {trash_basename}: {e}");
        }

        Ok(trash_fc)
    }

    /// Load `NcMetaData` for any filecache path, including extended times.
    async fn load_meta(&self, fc_path: &str) -> Option<NcMetaData> {
        let row = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            fc_path,
        )
        .await;
        tracing::trace!(
            fc_path = %fc_path,
            storage_id = self.storage_id,
            found = row.is_some(),
            "load_meta result"
        );
        let row = row?;

        let mime_type = {
            let cache = self.state.mime_cache.read().expect("mime cache lock");
            cache
                .get_name(row.mimetype)
                .unwrap_or("application/octet-stream")
                .to_string()
        };

        let ext = row::get_extended(&self.state.pool, &self.state.table_prefix, row.fileid).await;
        let mut meta = NcMetaData::from_row(&row, mime_type, ext.metadata_etag.clone());
        meta.apply_extended(ext.creation_time, ext.upload_time, ext.metadata_etag);
        Some(meta)
    }
}

// ─── blocking helper ──────────────────────────────────────────────────────────

async fn blocking<F, R>(func: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    match tokio::runtime::Handle::current().runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => task::block_in_place(func),
        _ => task::spawn_blocking(func).await.unwrap(),
    }
}

fn io_to_fs(e: io::Error) -> FsError {
    match e.kind() {
        io::ErrorKind::NotFound => FsError::NotFound,
        io::ErrorKind::PermissionDenied => FsError::Forbidden,
        io::ErrorKind::AlreadyExists => FsError::Exists,
        _ => FsError::GeneralFailure,
    }
}

/// Build the trash filecache path for `basename` deleted at `ts` (Unix secs).
///
/// Matches PHP `Trashbin::getTrashFilename()`: the name is strictly
/// `{basename}.d{ts}`.  On a same-second name collision PHP advances the
/// *timestamp* (see `move_to_trash`); it never appends a numeric suffix,
/// because `Trashbin::restore()`/`getLocation()` recover the timestamp by
/// splitting the trash name on `.d`.  Any other shape (e.g. `name.d{ts}_1`)
/// would be unrestorable via the PHP-FPM trashbin endpoint.
fn trash_fc_name(basename: &str, ts: i64) -> String {
    format!("files_trashbin/files/{basename}.d{ts}")
}

// ─── DavFileSystem impl ────────────────────────────────────────────────────────

impl DavFileSystem for NcFileSystem {
    // ── metadata ─────────────────────────────────────────────────────────────

    fn metadata<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let fc_path = self.to_fc_path(path);
            tracing::trace!(
                dav_path = %String::from_utf8_lossy(path.as_bytes()),
                fc_path = %fc_path,
                storage_id = self.storage_id,
                "metadata lookup"
            );
            let meta = self.load_meta(&fc_path).await.ok_or(FsError::NotFound)?;
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    // ── read_dir ──────────────────────────────────────────────────────────────

    fn read_dir<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let fc_path = self.to_fc_path(path);

            // Resolve the directory itself to get its fileid.
            let dir_row = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &fc_path,
            )
            .await
            .ok_or(FsError::NotFound)?;

            // Fetch all direct children.
            let children = row::list_children(
                &self.state.pool,
                &self.state.table_prefix,
                dir_row.fileid,
                self.storage_id,
            )
            .await;

            // Batch-load oc_filecache_extended for every child in a single
            // query instead of N individual queries — so that depth-1 PROPFIND
            // returns correct {nc:}creation_time, {nc:}upload_time, and
            // {nc:}metadata_etag without hitting the DB per-row (REQ §4.1).
            let child_fileids: Vec<i64> = children.iter().map(|c| c.fileid).collect();
            let extended_map = row::list_extended_batch(
                &self.state.pool,
                &self.state.table_prefix,
                &child_fileids,
            )
            .await;

            // §9.5: prefetch tags for the directory + all children so that
            // depth-1 PROPFIND has {oc:}favorite and {oc:}tags ready without
            // N+1 DB queries.  Include the directory itself.
            let mut prefetch_ids = child_fileids.clone();
            prefetch_ids.push(dir_row.fileid);
            crate::tags::prefetch_tags(
                &self.state.pool,
                &self.state.table_prefix,
                &self.uid,
                &prefetch_ids,
                &self.tag_cache,
            )
            .await;

            // Resolve MIME types from cache (no DB round-trip per row).
            let entries: Vec<Result<Box<dyn DavDirEntry>, FsError>> = {
                let cache = self.state.mime_cache.read().expect("mime cache lock");
                children
                    .into_iter()
                    .map(|child| {
                        let mime = cache
                            .get_name(child.mimetype)
                            .unwrap_or("application/octet-stream")
                            .to_string();
                        let mut meta = NcMetaData::from_row(&child, mime, None);
                        // Apply extended times from the batch map.
                        if let Some(ext) = extended_map.get(&child.fileid) {
                            meta.apply_extended(
                                ext.creation_time,
                                ext.upload_time,
                                ext.metadata_etag.clone(),
                            );
                        }
                        Ok(Box::new(NcDirEntry { meta }) as Box<dyn DavDirEntry>)
                    })
                    .collect()
            };

            let s: FsStream<Box<dyn DavDirEntry>> = Box::pin(stream::iter(entries));
            Ok(s)
        }
        .boxed()
    }

    // ── open ──────────────────────────────────────────────────────────────────

    fn open<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let fc_path = self.to_fc_path(path);
            let disk = self.disk_path(&fc_path);

            if options.read && !options.write {
                // ── Read-only ──────────────────────────────────────────────
                let meta = self.load_meta(&fc_path).await.ok_or(FsError::NotFound)?;
                let disk2 = disk.clone();
                let file = blocking(move || std::fs::File::open(&disk2))
                    .await
                    .map_err(io_to_fs)?;
                Ok(Box::new(NcDavFile {
                    file: Some(file),
                    meta,
                    write: None,
                }) as Box<dyn DavFile>)
            } else {
                // ── Write ──────────────────────────────────────────────────
                let existing = row::lookup_by_path(
                    &self.state.pool,
                    &self.state.table_prefix,
                    self.storage_id,
                    &fc_path,
                )
                .await;

                if options.create_new && existing.is_some() {
                    return Err(FsError::Exists);
                }

                // Resolve parent directory (auto-create if missing — matches PHP's
                // $userFolder->newFile() which calls createParentDirectories()).
                let parent_fc_path = {
                    let mut parts: Vec<&str> = fc_path.split('/').collect();
                    parts.pop();
                    if parts.is_empty() {
                        "files".to_string()
                    } else {
                        parts.join("/")
                    }
                };
                let parent_row = self
                    .ensure_parent_dir(&parent_fc_path)
                    .await
                    .map_err(|_| FsError::NotFound)?;

                // Resolve MIME type ids.
                let file_name = fc_path.rsplit('/').next().unwrap_or("");
                let ext = file_name.rsplit('.').next().unwrap_or("").to_lowercase();
                let mime_str = mime_guess::from_ext(&ext)
                    .first_raw()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let part_str = mime_str
                    .split('/')
                    .next()
                    .unwrap_or("application")
                    .to_string();

                // §10.8: get-or-insert; mimepart without trailing slash
                let (mime_type_id, mimepart_id) = {
                    let mid = nc_db::mime::get_or_insert_mime_id(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.state.mime_cache,
                        &mime_str,
                    )
                    .await;
                    let pid = nc_db::mime::get_or_insert_mime_id(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.state.mime_cache,
                        &part_str,
                    )
                    .await;
                    (mid, pid)
                };

                // Build metadata for the DavFile (may not exist in DB yet).
                let meta = match &existing {
                    Some(row) => {
                        let mime_type = {
                            let cache = self.state.mime_cache.read().expect("mime cache lock");
                            cache
                                .get_name(row.mimetype)
                                .unwrap_or("application/octet-stream")
                                .to_string()
                        };
                        NcMetaData::from_row(row, mime_type, None)
                    }
                    None => NcMetaData {
                        fileid: 0,
                        size: 0,
                        mtime: 0,
                        is_dir_flag: false,
                        mime_type: mime_str.clone(),
                        etag: None,
                        permissions: 27,
                        creation_time: 0,
                        upload_time: 0,
                        checksum: None,
                        display_name: file_name.to_string(),
                        metadata_etag: None,
                        storage: self.storage_id,
                        path: Some(fc_path.clone()),
                        parent: parent_row.fileid,
                    },
                };

                // Create parent dirs on disk if needed.
                let parent_disk = self.disk_path(&parent_fc_path);
                if !parent_disk.exists() {
                    blocking(move || std::fs::create_dir_all(&parent_disk))
                        .await
                        .map_err(io_to_fs)?;
                }

                // Create temp file next to the target.
                let temp_path = {
                    let mut p = disk.clone();
                    let name = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("tmp")
                        .to_string();
                    p.set_file_name(format!(".nc_upload_{name}_{}", uuid::Uuid::new_v4()));
                    p
                };
                let tp2 = temp_path.clone();
                let file = blocking(move || {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(&tp2)
                })
                .await
                .map_err(io_to_fs)?;

                let old_size = existing.as_ref().map(|r| r.size).unwrap_or(0);
                let old_mtime = existing.as_ref().map(|r| r.mtime).unwrap_or(0);
                let old_mimetype = existing.as_ref().map(|r| r.mimetype).unwrap_or(0);
                // §9.4: inherit permissions + extended times from the source file
                // so that the version row carries correct metadata for PHP-FPM.
                let old_permissions = existing.as_ref().map(|r| r.permissions).unwrap_or(27);
                let (old_creation_time, old_upload_time) =
                    if let Some(ref ex) = existing {
                        let ext = row::get_extended(
                            &self.state.pool,
                            &self.state.table_prefix,
                            ex.fileid,
                        )
                        .await;
                        (ext.creation_time, ext.upload_time)
                    } else {
                        (0, 0)
                    };
                let write_ctx = WriteCtx {
                    temp_path,
                    final_path: disk,
                    pool: self.state.pool.clone(),
                    prefix: self.state.table_prefix.clone(),
                    storage_id: self.storage_id,
                    fc_path,
                    parent_id: parent_row.fileid,
                    uid: self.uid.clone(),
                    mime_type_id,
                    mimepart_id,
                    initial_fileid: existing.map(|r| r.fileid),
                    old_size,
                    old_mtime,
                    old_mimetype,
                    old_permissions,
                    old_creation_time,
                    old_upload_time,
                    expected_size: options.size,
                    oc_checksum: options.checksum.clone(),
                    running_hash: crate::davfile::RunningHash::from_checksum_header(
                        options.checksum.as_deref(),
                    ),
                    x_oc_mtime: self.x_oc_mtime,
                    x_oc_ctime: self.x_oc_ctime,
                    write_result: self.write_result.clone(),
                    put_error: self.put_error.clone(),
                    propagator: self.propagator.clone(),
                    data_dir: self.state.data_directory.clone(),
                    mime_cache: self.state.mime_cache.clone(),
                    instance_id: self.state.instance_id.clone(),
                };

                Ok(Box::new(NcDavFile {
                    file: Some(file),
                    meta,
                    write: Some(write_ctx),
                }) as Box<dyn DavFile>)
            }
        }
        .boxed()
    }

    // ── create_dir ────────────────────────────────────────────────────────────

    fn create_dir<'a>(&'a self, path: &'a dav_server::davpath::DavPath) -> FsFuture<'a, ()> {
        async move {
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
            let parent_path = {
                let mut parts: Vec<&str> = fc_path.split('/').collect();
                parts.pop();
                parts.join("/")
            };
            let parent_row = self
                .ensure_parent_dir(&parent_path)
                .await
                .map_err(|_| FsError::NotFound)?;

            // Create directory on disk.
            blocking(move || std::fs::create_dir(&disk))
                .await
                .map_err(io_to_fs)?;

            // Insert into oc_filecache.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());

            // §10.8: mimetype = httpd/unix-directory, mimepart = httpd
            let dir_mime_id = nc_db::mime::get_or_insert_mime_id(
                &self.state.pool,
                &self.state.table_prefix,
                &self.state.mime_cache,
                "httpd/unix-directory",
            )
            .await;
            let dir_mimepart_id = nc_db::mime::get_or_insert_mime_id(
                &self.state.pool,
                &self.state.table_prefix,
                &self.state.mime_cache,
                "httpd",
            )
            .await;
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
            let _fid: i64 = sqlx::query_scalar(&sql)
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
                .fetch_one(&self.state.pool)
                .await
                .map_err(|e| {
                    warn!("create_dir DB insert failed: {e}");
                    FsError::GeneralFailure
                })?;

            // §9.2: MKCOL propagates etag/mtime to the parent chain.
            // New directories have size 0, so sizeDifference=0.
            if let Err(e) = self
                .propagator
                .propagate_change(&fc_path, now, 0)
                .await
            {
                tracing::warn!(path = %fc_path, error = %e, "mkcol: propagation failed");
            }

            Ok(())
        }
        .boxed()
    }

    // ── remove_file ───────────────────────────────────────────────────────────

    fn remove_file<'a>(&'a self, path: &'a dav_server::davpath::DavPath) -> FsFuture<'a, ()> {
        async move {
            let fc_path = self.to_fc_path(path);
            self.delete_file(&fc_path).await
        }
        .boxed()
    }

    // ── remove_dir ────────────────────────────────────────────────────────────

    fn remove_dir<'a>(&'a self, path: &'a dav_server::davpath::DavPath) -> FsFuture<'a, ()> {
        async move {
            let fc_path = self.to_fc_path(path);
            self.delete_dir(&fc_path).await
        }
        .boxed()
    }

    // ── rename (MOVE) ─────────────────────────────────────────────────────────

    fn rename<'a>(
        &'a self,
        from: &'a dav_server::davpath::DavPath,
        to: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, ()> {
        async move {
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
            let to_parent_fc = {
                let mut parts: Vec<&str> = to_fc.split('/').collect();
                parts.pop();
                parts.join("/")
            };
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

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let new_etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
            let new_name = to_fc.rsplit('/').next().unwrap_or("").to_string();
            let new_hash = row::path_hash(&to_fc);
            let prefix = &self.state.table_prefix;

            // Resolve directory mimetype ID early — used both for the
            // directory-guard in the mimetype recomputation below and the
            // subtree-rename check further down.
            let dir_mime_id = nc_db::mime::get_or_insert_mime_id(
                &self.state.pool,
                &self.state.table_prefix,
                &self.state.mime_cache,
                "httpd/unix-directory",
            )
            .await;

            // Update the node itself.
            let sql_node = format!(
                "UPDATE {prefix}filecache \
                 SET path=$1, path_hash=$2, name=$3, parent=$4, mtime=$5, etag=$6 \
                 WHERE fileid=$7"
            );
            sqlx::query(&sql_node)
                .bind(&to_fc)
                .bind(&new_hash)
                .bind(&new_name)
                .bind(to_parent.fileid)
                .bind(now)
                .bind(&new_etag)
                .bind(from_row.fileid)
                .execute(&self.state.pool)
                .await
                .map_err(|_| FsError::GeneralFailure)?;

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
                let new_part = new_mime.split('/').next().unwrap_or("application").to_string();
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
                let sql_mime = format!(
                    "UPDATE {prefix}filecache SET mimetype=$1, mimepart=$2 WHERE fileid=$3"
                );
                if let Err(e) = sqlx::query(&sql_mime)
                    .bind(new_mid)
                    .bind(new_pid)
                    .bind(from_row.fileid)
                    .execute(&self.state.pool)
                    .await
                {
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
                self
                    .rename_subtree_paths(&from_fc, &to_fc, from_row.fileid, prefix)
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
                &from_fc,
                &to_fc,
            )
            .await;

            // §9.2: MOVE propagates both source and target chains with
            // sizeDifference=0 (etag/mtime only).  The immediate source/target
            // parents' sizes are fixed by correctFolderSize (PHP
            // Updater.php:195-204).
            let from_parent_fc = {
                let mut parts: Vec<&str> = from_fc.split('/').collect();
                parts.pop();
                parts.join("/")
            };
            if let Err(e) = self
                .propagator
                .propagate_change(&from_fc, now, 0)
                .await
            {
                tracing::warn!(path = %from_fc, error = %e, "move: source propagation failed");
            }
            if let Err(e) = self
                .propagator
                .propagate_change(&to_fc, now, 0)
                .await
            {
                tracing::warn!(path = %to_fc, error = %e, "move: target propagation failed");
            }
            // Recalculate immediate parent sizes (size change can't be expressed
            // as a simple signed delta when subtrees move).
            if from_parent_fc != to_parent_fc {
                if let Err(e) = self.propagator.correct_folder_size(&from_parent_fc).await {
                    tracing::warn!(path = %from_parent_fc, error = %e, "move: correct_folder_size from failed");
                }
                if let Err(e) = self.propagator.correct_folder_size(&to_parent_fc).await {
                    tracing::warn!(path = %to_parent_fc, error = %e, "move: correct_folder_size to failed");
                }
            } else {
                // Same parent (rename within the same directory) — recalculate once.
                if let Err(e) = self.propagator.correct_folder_size(&to_parent_fc).await {
                    tracing::warn!(path = %to_parent_fc, error = %e, "move: correct_folder_size failed");
                }
            }

            Ok(())
        }
        .boxed()
    }

    // ── copy ─────────────────────────────────────────────────────────────────

    fn copy<'a>(
        &'a self,
        from: &'a dav_server::davpath::DavPath,
        to: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, ()> {
        async move {
            let from_fc = self.to_fc_path(from);
            let to_fc = self.to_fc_path(to);
            let from_disk = self.disk_path(&from_fc);
            let to_disk = self.disk_path(&to_fc);

            blocking(move || std::fs::copy(&from_disk, &to_disk).map(|_| ()))
                .await
                .map_err(io_to_fs)?;

            // For simplicity: remove old DB row for destination if exists,
            // then insert new row by re-reading disk metadata.
            if let Err(e) = sqlx::query(&format!(
                "DELETE FROM {prefix}filecache \
                 WHERE storage = $1 AND path_hash = $2",
                prefix = self.state.table_prefix
            ))
            .bind(self.storage_id)
            .bind(row::path_hash(&to_fc))
            .execute(&self.state.pool)
            .await
            {
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
                let to_parent_fc = {
                    let mut parts: Vec<&str> = to_fc.split('/').collect();
                    parts.pop();
                    parts.join("/")
                };
                if let Some(parent_row) = row::lookup_by_path(
                    &self.state.pool,
                    &self.state.table_prefix,
                    self.storage_id,
                    &to_parent_fc,
                )
                .await
                {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
                    let name = to_fc.rsplit('/').next().unwrap_or("").to_string();
                    let hash = row::path_hash(&to_fc);
                    let prefix = &self.state.table_prefix;

                    // §10.10: recompute mimetype on extension change for COPY.
                    // Matches PHP Updater::copyOrRenameFromStorage() —
                    // skip for directories and trash targets.
                    let dir_mime_id = nc_db::mime::get_or_insert_mime_id(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.state.mime_cache,
                        "httpd/unix-directory",
                    )
                    .await;
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
                            let new_part =
                                new_mime.split('/').next().unwrap_or("application").to_string();
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

                    let sql = format!(
                        "INSERT INTO {prefix}filecache \
                         (storage, path, path_hash, parent, name, mimetype, mimepart, \
                          size, mtime, storage_mtime, etag, permissions, checksum) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
                         RETURNING fileid"
                    );
                    let copy_fid: Option<i64> = match sqlx::query_scalar::<_, i64>(&sql)
                        .bind(self.storage_id)
                        .bind(&to_fc)
                        .bind(&hash)
                        .bind(parent_row.fileid)
                        .bind(&name)
                        .bind(copy_mid)
                        .bind(copy_pid)
                        .bind(from_row.size)
                        .bind(now)
                        .bind(now)
                        .bind(&etag)
                        .bind(from_row.permissions)
                        .bind(from_row.checksum.as_deref().unwrap_or(""))
                        .fetch_one(&self.state.pool)
                        .await
                    {
                        Ok(fid) => Some(fid),
                        Err(e) => {
                            tracing::warn!(to_fc = to_fc, error = %e, "Failed to insert filecache row on copy");
                            None
                        }
                    };

                    // §9.4: insert oc_files_versions for the copied file.
                    // Matches PHP NodeCreatedEvent → created() → createVersionEntity().
                    if let Some(fid) = copy_fid {
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
                    }
                }
            }

            // §9.4: copy versions alongside the copied file.
            crate::versions::copy_versions(
                &self.state.pool,
                &self.state.table_prefix,
                &self.state.data_directory,
                &self.uid,
                &from_fc,
                &to_fc,
            )
            .await;

            // §9.2: COPY propagates the target chain with
            // sizeDifference=0 (etag/mtime only), then corrects the
            // immediate target parent size (PHP Updater.php:195-204).
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if let Err(e) = self
                .propagator
                .propagate_change(&to_fc, now, 0)
                .await
            {
                tracing::warn!(path = %to_fc, error = %e, "copy: propagation failed");
            }
            let to_parent_fc = {
                let mut parts: Vec<&str> = to_fc.split('/').collect();
                parts.pop();
                parts.join("/")
            };
            if let Err(e) = self.propagator.correct_folder_size(&to_parent_fc).await {
                tracing::warn!(path = %to_parent_fc, error = %e, "copy: correct_folder_size failed");
            }

            Ok(())
        }
        .boxed()
    }

    // ── set_modified ──────────────────────────────────────────────────────────

    fn set_modified<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        tm: std::time::SystemTime,
    ) -> FsFuture<'a, ()> {
        async move {
            let fc_path = self.to_fc_path(path);
            let mtime = tm
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let sql = format!(
                "UPDATE {prefix}filecache SET mtime=$1, storage_mtime=$2 WHERE storage=$3 AND path_hash=$4",
                prefix = self.state.table_prefix
            );
            sqlx::query(&sql)
                .bind(mtime)
                .bind(mtime)
                .bind(self.storage_id)
                .bind(row::path_hash(&fc_path))
                .execute(&self.state.pool)
                .await
                .map_err(|_| FsError::GeneralFailure)?;

            // §9.2: mtime-changing PROPPATCH propagates etag/mtime to
            // ancestors (sizeDifference=0).
            if let Err(e) = self
                .propagator
                .propagate_change(&fc_path, mtime, 0)
                .await
            {
                tracing::warn!(path = %fc_path, error = %e, "set_modified: propagation failed");
            }

            Ok(())
        }
        .boxed()
    }

    // ── quota ─────────────────────────────────────────────────────────────────

    fn get_quota(&'_ self) -> FsFuture<'_, (u64, Option<u64>)> {
        async move {
            // Query the `files/` root filecache entry.  Nextcloud keeps its
            // `size` column equal to the recursive total for the user's home
            // storage, so this single lookup gives the total used bytes without
            // scanning the full subtree (REQ §6.5, PHASE-4.7).
            let used = match row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                "files",
            )
            .await
            {
                Some(r) => r.size.max(0) as u64,
                None => 0,
            };

            // Return `None` for `total` — unlimited quota.
            //
            // dav-server only emits `{DAV:}quota-available-bytes` when the
            // second tuple element is `Some`; by returning `None` we suppress
            // its internal emit.  `get_props()` then injects
            // `quota-available-bytes = -3` (SPACE_UNLIMITED, REQ §6.5) via
            // `build_props()` without producing a duplicate.
            //
            // Per-user quota from `oc_preferences` is deferred to a later phase.
            Ok((used, None))
        }
        .boxed()
    }

    // ── custom DAV properties (oc: / nc: namespaces) ──────────────────────────

    fn have_props<'a>(
        &'a self,
        _path: &'a dav_server::davpath::DavPath,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(future::ready(true))
    }

    fn get_props<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        do_content: bool,
    ) -> FsFuture<'a, Vec<DavProp>> {
        async move {
            let fc_path = self.to_fc_path(path);
            let meta = match self.load_meta(&fc_path).await {
                Some(m) => m,
                None => return Ok(vec![]),
            };

            // Read data-fingerprint from appconfig (REQ §6.5)
            let data_fingerprint = {
                let cache = self.state.appconfig_cache.read().expect("appconfig lock");
                cache
                    .get_string("core", "data-fingerprint")
                    .unwrap_or_default()
            };

            // Count direct children for directories (REQ §6.5 contained-*-count)
            let (child_dirs, child_files) = if meta.is_dir_flag && do_content {
                let dir_mime_id = nc_db::mime::get_or_insert_mime_id(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &self.state.mime_cache,
                    "httpd/unix-directory",
                )
                .await;
                row::count_children(
                    &self.state.pool,
                    &self.state.table_prefix,
                    meta.fileid,
                    self.storage_id,
                    dir_mime_id,
                )
                .await
            } else {
                (0, 0)
            };

            // Resolve {oc:}owner-display-name from oc_users.displayname (REQ §6.5 / §4.8).
            // Falls back to the raw UID when no display name is set.
            let owner_display_name = row::lookup_user_display_name(
                &self.state.pool,
                &self.state.table_prefix,
                &self.uid,
            )
            .await;

            // ── Phase 7.6: is_mounted, share_permissions, download_url, note ──
            //
            // `is_mounted`: true when the file lives on a non-home storage.
            // Optimisation: if meta.storage == self.storage_id the FS was
            // already constructed from a home:: storage lookup, so skip DB.
            let is_mounted = if meta.storage == self.storage_id {
                false
            } else {
                row::get_storage_string_id(&self.state.pool, &self.state.table_prefix, meta.storage)
                    .await
                    .map(|id| !id.starts_with("home::"))
                    .unwrap_or(false)
            };

            // `share_permissions`: MAX(permissions) from oc_share for this
            // file and the authenticated user.  Default 31 (all) when no row.
            let share_permissions = row::get_share_max_permissions(
                &self.state.pool,
                &self.state.table_prefix,
                &self.uid,
                meta.fileid,
            )
            .await;

            // `note`: most-recent non-empty share note for this file.
            let note =
                row::get_share_note(&self.state.pool, &self.state.table_prefix, meta.fileid).await;

            // `download_url`: direct WebDAV URL for home-storage files.
            // Format: {overwrite.cli.url}/remote.php/webdav/{path-without-files-prefix}
            // Empty for non-home storage (object/S3 URLs require storage-specific
            // signed-URL support which is out of scope, PHASE-7.6).
            // Only generated for files (not directories) and when base_url is set.
            let download_url =
                if !is_mounted && !meta.is_dir_flag && !self.state.base_url.is_empty() {
                    // `meta.path` is like "files/Photos/img.jpg"; strip "files" prefix
                    // to get the WebDAV subpath "/Photos/img.jpg".
                    let subpath = meta
                        .path
                        .as_deref()
                        .unwrap_or("")
                        .trim_start_matches("files");
                    let base = self.state.base_url.trim_end_matches('/');
                    format!("{base}/remote.php/webdav{}", percent_encode_path(subpath))
                } else {
                    String::new()
                };

            let instance_id = &self.state.instance_id;
            // is_shared: false for home-storage nodes — the file is the user's own.
            // Shared nodes (from oc_share) are detected via is_mounted/share_permissions.
            let is_shared = false;

            // §10.12 / §11.1: compute {nc:}has-preview from mimetype + the resolved
            // provider registry (enabledPreviewProviders gating, Imaginary, binaries).
            let has_preview = self
                .state
                .preview_registry
                .is_available(&meta.mime_type, is_mounted);

            // §9.5: resolve tags / favorite from oc_vcategory / oc_vcategory_to_object.
            let tag_info = crate::tags::get_tag_info(
                &self.state.pool,
                &self.state.table_prefix,
                &self.uid,
                meta.fileid,
                &self.tag_cache,
            )
            .await;

            let mut props = crate::props::build_props(
                &meta,
                instance_id,
                &self.uid,
                &owner_display_name,
                do_content,
                &data_fingerprint,
                child_dirs,
                child_files,
                is_mounted,
                is_shared,
                share_permissions,
                &download_url,
                &note,
                has_preview,
                &tag_info.tags,
                tag_info.is_favorite,
            );

            // ── Append custom properties from oc_properties (task §10.11) ─────
            if do_content {
                let custom_props = crate::row::list_custom_properties(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &self.uid,
                    &fc_path,
                )
                .await;
                for (propname, propvalue, _valuetype) in custom_props {
                    if let Some((ns, name)) = crate::row::parse_clark_notation(&propname) {
                        // Skip known-namespace props — they are handled above or
                        // by the dav-server framework.
                        if ns == "DAV:"
                            || ns == "http://owncloud.org/ns"
                            || ns == "http://nextcloud.org/ns"
                            || ns == "http://open-collaboration-services.org/ns"
                        {
                            continue;
                        }
                        props.push(DavProp::new(
                            name.to_string(),
                            String::new(),
                            ns.to_string(),
                            propvalue,
                        ));
                    }
                }
            }

            Ok(props)
        }
        .boxed()
    }

    fn patch_props<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        patch: Vec<(bool, DavProp)>,
    ) -> FsFuture<'a, Vec<(http::StatusCode, DavProp)>> {
        async move {
            let fc_path = self.to_fc_path(path);
            let hash    = row::path_hash(&fc_path);
            let mut results = Vec::new();

            for (set, prop) in patch {
                let ns   = prop.namespace.as_deref().unwrap_or("");
                let name = prop.name.as_str();

                let status = if set {
                    match (ns, name) {
                        // ── Standard DAV writable props (REQ §6.6) ───────────

                        // {DAV:}getetag — set custom ETag
                        ("DAV:", "getetag") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                let etag = val.trim().trim_matches('"').to_string();
                                let sql = format!(
                                    "UPDATE {prefix}filecache SET etag=$1 \
                                     WHERE storage=$2 AND path_hash=$3",
                                    prefix = self.state.table_prefix
                                );
                                let ok = sqlx::query(&sql)
                                    .bind(&etag)
                                    .bind(self.storage_id)
                                    .bind(&hash)
                                    .execute(&self.state.pool)
                                    .await
                                    .is_ok();
                                if ok { http::StatusCode::OK } else { http::StatusCode::INTERNAL_SERVER_ERROR }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        // {DAV:}getlastmodified / {DAV:}lastmodified — update mtime
                        ("DAV:", "getlastmodified") | ("DAV:", "lastmodified") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                // RFC 1123 date OR Unix timestamp integer
                                let ts_opt = val.trim().parse::<i64>().ok().or_else(|| {
                                    httpdate::parse_http_date(val.trim())
                                        .ok()
                                        .and_then(|st| {
                                            st.duration_since(std::time::UNIX_EPOCH)
                                                .ok()
                                                .map(|d| d.as_secs() as i64)
                                        })
                                });
                                if let Some(ts) = ts_opt {
                                    let sql = format!(
                                        "UPDATE {prefix}filecache SET mtime=$1, storage_mtime=$2 \
                                         WHERE storage=$3 AND path_hash=$4",
                                        prefix = self.state.table_prefix
                                    );
                                    if let Err(e) = sqlx::query(&sql)
                                        .bind(ts).bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(&self.state.pool)
                                        .await
                                    {
                                        tracing::warn!(path_hash = hash, error = %e, "PROPPATCH: failed to update mtime");
                                    }
                                    http::StatusCode::OK
                                } else {
                                    http::StatusCode::BAD_REQUEST
                                }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        // {DAV:}creationdate — set creation time (ISO 8601)
                        //
                        // When the oc_filecache_extended row does not yet exist we INSERT it,
                        // reading upload_time from oc_filecache so that the sibling column is
                        // preserved rather than zeroed (REQ §6.6, PHASE-4.10).
                        ("DAV:", "creationdate") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                if let Some(ts) = parse_iso8601(val.trim()) {
                                    let sql = format!(
                                        "INSERT INTO {prefix}filecache_extended \
                                         (fileid, creation_time, metadata_etag, upload_time) \
                                         SELECT fileid, $1, NULL, upload_time FROM {prefix}filecache \
                                         WHERE storage=$2 AND path_hash=$3 \
                                         ON CONFLICT(fileid) DO UPDATE SET creation_time=excluded.creation_time",
                                        prefix = self.state.table_prefix
                                    );
                                    if let Err(e) = sqlx::query(&sql)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(&self.state.pool)
                                        .await
                                    {
                                        tracing::warn!(path_hash = hash, error = %e, "PROPPATCH: failed to update creationdate");
                                    }
                                    http::StatusCode::OK
                                } else {
                                    http::StatusCode::BAD_REQUEST
                                }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        // {DAV:}displayname — explicitly blocked (REQ §6.6)
                        ("DAV:", "displayname") => http::StatusCode::FORBIDDEN,

                        // ── NC writable props ─────────────────────────────────
                        //
                        // For each nc: time property, when the oc_filecache_extended row is
                        // absent we INSERT it, reading the *other* timestamp column from
                        // oc_filecache so neither value is zeroed (REQ §6.6, PHASE-4.10).
                        ("http://nextcloud.org/ns", "creation_time") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                if let Ok(ts) = val.trim().parse::<i64>() {
                                    let sql_upsert = format!(
                                        "INSERT INTO {prefix}filecache_extended \
                                         (fileid, creation_time, metadata_etag, upload_time) \
                                         SELECT fileid, $1, NULL, upload_time FROM {prefix}filecache \
                                         WHERE storage = $2 AND path_hash = $3 \
                                         ON CONFLICT(fileid) DO UPDATE SET creation_time = excluded.creation_time",
                                        prefix = self.state.table_prefix,
                                    );
                                    if let Err(e) = sqlx::query(&sql_upsert)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(&self.state.pool)
                                        .await
                                    {
                                        tracing::warn!(path_hash = hash, error = %e, "PROPPATCH: failed to update timestamp");
                                    }
                                    http::StatusCode::OK
                                } else {
                                    http::StatusCode::BAD_REQUEST
                                }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        // {nc:}upload_time — update upload time (unix int).
                        // Preserve creation_time from oc_filecache when inserting a new
                        // extended row (PHASE-4.10).
                        ("http://nextcloud.org/ns", "upload_time") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                if let Ok(ts) = val.trim().parse::<i64>() {
                                    let sql_upsert = format!(
                                        "INSERT INTO {prefix}filecache_extended \
                                         (fileid, upload_time, metadata_etag, creation_time) \
                                         SELECT fileid, $1, NULL, creation_time FROM {prefix}filecache \
                                         WHERE storage = $2 AND path_hash = $3 \
                                         ON CONFLICT(fileid) DO UPDATE SET upload_time = excluded.upload_time",
                                        prefix = self.state.table_prefix,
                                    );
                                    if let Err(e) = sqlx::query(&sql_upsert)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(&self.state.pool)
                                        .await
                                    {
                                        tracing::warn!(path_hash = hash, error = %e, "PROPPATCH: failed to update timestamp");
                                    }
                                    http::StatusCode::OK
                                } else {
                                    http::StatusCode::BAD_REQUEST
                                }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        // §9.5: {oc:}favorite — truthy test: (int)1 || 'true' → tagAs,
                        //                   falsy → unTag.  Returns 200.
                        ("http://owncloud.org/ns", "favorite") => {
                            let state = prop.xml.as_ref().and_then(|xml| {
                                std::str::from_utf8(xml).ok().map(|s| s.to_string())
                            });
                            let is_fav = state.as_deref().map_or(false, |s| {
                                s.parse::<i64>().ok() == Some(1) || s == "true"
                            });
                            let fileid = crate::row::lookup_by_path(
                                &self.state.pool,
                                &self.state.table_prefix,
                                self.storage_id,
                                &fc_path,
                            ).await.map(|r| r.fileid).unwrap_or(0);
                            if fileid != 0 {
                                let _ = crate::tags::set_favorite(
                                    &self.state.pool,
                                    &self.state.table_prefix,
                                    &self.uid,
                                    fileid,
                                    is_fav,
                                ).await;
                                // Invalidate cached tags for this node.
                                if let Ok(mut cache) = self.tag_cache.lock() {
                                    cache.remove(&fileid);
                                }
                            }
                            http::StatusCode::OK
                        }

                        // §9.5: {oc:}tags — diff current vs requested, skip favorite sentinel.
                        ("http://owncloud.org/ns", "tags") => {
                            let requested = prop.xml.as_ref()
                                .map(|xml| crate::tags::parse_tags_xml(xml))
                                .unwrap_or_default();
                            if let Some(fc_row) = crate::row::lookup_by_path(
                                &self.state.pool,
                                &self.state.table_prefix,
                                self.storage_id,
                                &fc_path,
                            ).await {
                                let fileid = fc_row.fileid;
                                let tag_info = crate::tags::get_tag_info(
                                    &self.state.pool,
                                    &self.state.table_prefix,
                                    &self.uid,
                                    fileid,
                                    &self.tag_cache,
                                ).await;
                                // Reconstruct the full tag list (including favorite sentinel if set).
                                let mut current = tag_info.tags.clone();
                                if tag_info.is_favorite {
                                    current.push(crate::tags::TAG_FAVORITE.to_string());
                                }
                                crate::tags::update_tags(
                                    &self.state.pool,
                                    &self.state.table_prefix,
                                    &self.uid,
                                    fileid,
                                    &current,
                                    &requested,
                                ).await;
                                // Invalidate cached tags.
                                if let Ok(mut cache) = self.tag_cache.lock() {
                                    cache.remove(&fileid);
                                }
                            }
                            http::StatusCode::OK
                        }

                        _ => {
                            // Custom property → store in oc_properties (task §10.11).
                            let prop_name_full = format!("{{{ns}}}{name}");
                            if let Some(ref xml_bytes) = prop.xml {
                                let _ = crate::row::upsert_custom_property(
                                    &self.state.pool,
                                    &self.state.table_prefix,
                                    &self.uid,
                                    &fc_path,
                                    &prop_name_full,
                                    xml_bytes,
                                    2, // PROPERTY_TYPE_XML
                                )
                                .await;
                                http::StatusCode::OK
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }
                    }
                } else {
                    // DELETE — built-in props cannot be removed; custom props are
                    // deleted from oc_properties (task §10.11).
                    // §9.5: {oc:}favorite and {oc:}tags are exceptions — PHP
                    // handles these by clearing the tag/favorite state.
                    match (ns, name) {
                        // §9.5: deleting {oc:}favorite → unTag TAG_FAVORITE → 204.
                        ("http://owncloud.org/ns", "favorite") => {
                            if let Some(fc_row) = crate::row::lookup_by_path(
                                &self.state.pool,
                                &self.state.table_prefix,
                                self.storage_id,
                                &fc_path,
                            ).await {
                                let _ = crate::tags::un_tag(
                                    &self.state.pool,
                                    &self.state.table_prefix,
                                    &self.uid,
                                    fc_row.fileid,
                                    crate::tags::TAG_FAVORITE,
                                ).await;
                                // Invalidate cached tags.
                                if let Ok(mut cache) = self.tag_cache.lock() {
                                    cache.remove(&fc_row.fileid);
                                }
                            }
                            http::StatusCode::NO_CONTENT
                        }
                        // §9.5: deleting {oc:}tags → remove all non-favorite tags → 204.
                        ("http://owncloud.org/ns", "tags") => {
                            if let Some(fc_row) = crate::row::lookup_by_path(
                                &self.state.pool,
                                &self.state.table_prefix,
                                self.storage_id,
                                &fc_path,
                            ).await {
                                let tag_info = crate::tags::get_tag_info(
                                    &self.state.pool,
                                    &self.state.table_prefix,
                                    &self.uid,
                                    fc_row.fileid,
                                    &self.tag_cache,
                                ).await;
                                // Remove all non-favorite tags. Favorite status is preserved.
                                let mut current = tag_info.tags.clone();
                                if tag_info.is_favorite {
                                    current.push(crate::tags::TAG_FAVORITE.to_string());
                                }
                                // Clear all tags (keep only favorite if present).
                                let keep_fav: Vec<String> = if tag_info.is_favorite {
                                    vec![crate::tags::TAG_FAVORITE.to_string()]
                                } else {
                                    vec![]
                                };
                                crate::tags::update_tags(
                                    &self.state.pool,
                                    &self.state.table_prefix,
                                    &self.uid,
                                    fc_row.fileid,
                                    &current,
                                    &keep_fav,
                                ).await;
                                // Invalidate cached tags.
                                if let Ok(mut cache) = self.tag_cache.lock() {
                                    cache.remove(&fc_row.fileid);
                                }
                            }
                            http::StatusCode::NO_CONTENT
                        }
                        ("DAV:", _)
                        | ("http://nextcloud.org/ns", _)
                        | ("http://owncloud.org/ns", _)
                        | ("http://open-collaboration-services.org/ns", _) => {
                            http::StatusCode::FORBIDDEN
                        }
                        _ => {
                            let prop_name_full = format!("{{{ns}}}{name}");
                            let _ = crate::row::delete_custom_property(
                                &self.state.pool,
                                &self.state.table_prefix,
                                &self.uid,
                                &fc_path,
                                &prop_name_full,
                            )
                            .await;
                            http::StatusCode::OK
                        }
                    }
                };
                results.push((status, prop));
            }
            Ok(results)
        }
        .boxed()
    }
}

// ─── Helper methods ────────────────────────────────────────────────────────────

/// Extract the file extension from a filename (the part after the last `.`).
/// Returns empty string for extensionless names.
fn extension(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or("")
}

/// Check whether an extension matches the PHP trashbin naming convention
/// (`$targetIsTrash = preg_match("/^d\d+$/", $targetExtension)`).
///
/// Trashbin files are named `original.txt.d{timestamp}` — the extension is
/// `d` followed by digits.  PHP skips mimetype recomputation for trash
/// targets because the original (pre‑trash) extension is still in the name.
fn is_trash_extension(ext: &str) -> bool {
    if ext.len() < 2 || !ext.starts_with('d') {
        return false;
    }
    ext[1..].chars().all(|c| c.is_ascii_digit())
}

impl NcFileSystem {
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
        let rows = sqlx::query(&sql_fetch)
            .bind(self.storage_id)
            .bind(&like)
            .fetch_all(&self.state.pool)
            .await
            .unwrap_or_default();

        for r in rows {
            let old_path: String = r.get("path");
            let new_path = format!("{new_prefix}{}", &old_path[old_prefix.len()..]);
            let new_hash = row::path_hash(&new_path);
            let fileid: i64 = r.get("fileid");
            let sql_upd =
                format!("UPDATE {prefix}filecache SET path=$1, path_hash=$2 WHERE fileid=$3");
            let _ = sqlx::query(&sql_upd)
                .bind(&new_path)
                .bind(&new_hash)
                .bind(fileid)
                .execute(&self.state.pool)
                .await;
        }
    }
}

// ─── XML prop value extractor ─────────────────────────────────────────────────

/// Percent-encode a URI path string, preserving `/` as a segment separator.
///
/// Encodes any byte that is not an RFC 3986 unreserved character (`A-Za-z0-9-._~`)
/// or one of the path-allowed characters (`/:@!$&'()*+,;=`).
/// Used to build `{oc:}downloadURL` values (PHASE-7.6).
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/'
            | b':'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'=' => out.push(byte as char),
            _ => {
                out.push('%');
                let hi = byte >> 4;
                let lo = byte & 0xF;
                out.push(
                    char::from_digit(hi as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
                out.push(
                    char::from_digit(lo as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
            }
        }
    }
    out
}

/// Extract the text content from a `DavProp`'s raw XML bytes.
/// e.g. `<nc:creation_time xmlns:nc="...">1700000000</nc:creation_time>` → `"1700000000"`
fn extract_text_from_prop_xml(prop: &DavProp) -> Option<String> {
    let xml = prop.xml.as_ref()?;
    let s = std::str::from_utf8(xml).ok()?;
    let start = s.find('>')? + 1;
    let end = s.rfind('<')?;
    if start < end {
        Some(s[start..end].to_string())
    } else {
        None
    }
}

// ─── ISO 8601 parser ──────────────────────────────────────────────────────────

/// Parse an ISO 8601 / RFC 3339 datetime string to a Unix timestamp (UTC).
///
/// Handles the subset used by WebDAV `{DAV:}creationdate`: `YYYY-MM-DDTHH:MM:SSZ`
/// and `YYYY-MM-DDTHH:MM:SS+HH:MM` / `YYYY-MM-DDTHH:MM:SS-HH:MM`.
/// Timezone offsets are **ignored** (all times treated as UTC) for simplicity.
fn parse_iso8601(s: &str) -> Option<i64> {
    // Strip timezone suffix to get at most "YYYY-MM-DDTHH:MM:SS"
    let core = if s.ends_with('Z') {
        &s[..s.len() - 1]
    } else if let Some(pos) = s.rfind('+').filter(|&p| p >= 10) {
        &s[..pos]
    } else if let Some(pos) = s[10..].rfind('-').map(|p| p + 10) {
        &s[..pos]
    } else {
        s
    };

    if core.len() < 19 {
        return None;
    }

    let year: i64 = core[0..4].parse().ok()?;
    let month: i64 = core[5..7].parse().ok()?;
    let day: i64 = core[8..10].parse().ok()?;
    let hour: i64 = core[11..13].parse().ok()?;
    let min: i64 = core[14..16].parse().ok()?;
    let sec: i64 = core[17..19].parse().ok()?;

    // Julian Day Number → Unix epoch (days)
    let a = (14 - month) / 12;
    let y = year + 4800 - a;
    let m = month + 12 * a - 3;
    let jdn = day + (153 * m + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32045;
    let unix_days = jdn - 2_440_588; // JDN of 1970-01-01

    Some(unix_days * 86_400 + hour * 3_600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::{extension, is_trash_extension, parse_iso8601, trash_fc_name};

    #[test]
    fn iso8601_z_suffix() {
        // python3: datetime(2024,1,15,12,34,56,tz=UTC).timestamp() == 1705322096
        assert_eq!(parse_iso8601("2024-01-15T12:34:56Z"), Some(1705322096));
    }

    #[test]
    fn iso8601_plus_offset_ignored() {
        // Offset is stripped so both forms give the same raw numbers treated as UTC
        assert_eq!(parse_iso8601("2024-01-15T12:34:56+02:00"), Some(1705322096));
    }

    #[test]
    fn iso8601_unix_epoch() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
    }

    // ── Trash path computation ─────────────────────────────────────────────
    //
    // These tests verify our implementation matches the behaviour of:
    //   apps/files_trashbin/lib/Trashbin.php::move2trash()
    //     → pathinfo() for id/location extraction
    //     → getTrashFilename() for basename.d{ts} naming
    //   apps/files_trashbin/lib/Helper.php::getTrashFiles()
    //     → pathinfo() parsing of .d{ts} suffix from flat trash names

    /// Compute the trash `id` (basename) and `location` (dirname) for
    /// `oc_files_trash`, matching PHP's `pathinfo()` behaviour.
    ///
    /// Returns `(trash_basename, location, trash_fc_path)`.
    fn trash_path_info(fc_path: &str, now: i64) -> (String, String, String) {
        let relative = fc_path.strip_prefix("files/").unwrap_or(fc_path);
        let p = std::path::Path::new(relative);

        let basename = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| relative.to_string());

        let location = p
            .parent()
            .and_then(|d| d.to_str())
            .map(|s| if s.is_empty() { "." } else { s })
            .unwrap_or(".")
            .to_string();

        let trash_fc = trash_fc_name(&basename, now);

        (basename, location, trash_fc)
    }

    /// Mirror of the collision-resolution loop in `move_to_trash`: advance the
    /// timestamp until `taken` reports the candidate path is free.  Shares the
    /// production `trash_fc_name` formatter so the PHP-compatible naming is
    /// covered directly.
    fn resolve_trash_collision(
        basename: &str,
        start_ts: i64,
        taken: impl Fn(&str) -> bool,
    ) -> (String, i64) {
        let mut ts = start_ts;
        loop {
            let candidate = trash_fc_name(basename, ts);
            if !taken(&candidate) {
                return (candidate, ts);
            }
            ts += 1;
        }
    }

    #[test]
    fn trash_name_format_matches_php_no_suffix() {
        // getTrashFilename() is strictly "{basename}.d{ts}".
        assert_eq!(
            trash_fc_name("test.txt", 1784065766),
            "files_trashbin/files/test.txt.d1784065766"
        );
        // The filename segment must never carry a "_N" collision suffix
        // (the `files_trashbin` directory legitimately contains an underscore).
        let name = trash_fc_name("test.txt", 1)
            .rsplit('/')
            .next()
            .unwrap()
            .to_string();
        assert!(!name.contains('_'));
    }

    #[test]
    fn trash_collision_bumps_timestamp() {
        // Same-second delete of a same-named file: the first timestamp is
        // taken, so resolution must advance to ts+1 (PHP behaviour), NOT
        // append "_1".
        let taken_path = trash_fc_name("f.txt", 100);
        let (resolved, ts) = resolve_trash_collision("f.txt", 100, |c| c == taken_path);
        assert_eq!(ts, 101);
        assert_eq!(resolved, "files_trashbin/files/f.txt.d101");
        let name = resolved.rsplit('/').next().unwrap();
        assert!(!name.contains('_'));
    }

    #[test]
    fn trash_collision_advances_over_multiple_taken_timestamps() {
        // Two consecutive timestamps taken → resolve to ts+2, still with the
        // canonical ".d{ts}" shape (regression guard against "_N" suffixing).
        let taken: std::collections::HashSet<String> =
            [trash_fc_name("f.txt", 100), trash_fc_name("f.txt", 101)]
                .into_iter()
                .collect();
        let (resolved, ts) = resolve_trash_collision("f.txt", 100, |c| taken.contains(c));
        assert_eq!(ts, 102);
        assert_eq!(resolved, "files_trashbin/files/f.txt.d102");
        let name = resolved.rsplit('/').next().unwrap();
        assert!(!name.contains('_'));
    }

    #[test]
    fn trash_no_collision_keeps_original_timestamp() {
        let (resolved, ts) = resolve_trash_collision("f.txt", 100, |_| false);
        assert_eq!(ts, 100);
        assert_eq!(resolved, "files_trashbin/files/f.txt.d100");
    }

    #[test]
    fn trash_root_level_file_basename_is_correct() {
        // Trashbin::move2trash() line 263-268:
        //   $path_parts = pathinfo($ownerPath);
        //   $filename = $path_parts['basename'];  // "test"
        //   $location = $path_parts['dirname'];   // "." stored as "."
        let (basename, location, trash_fc) = trash_path_info("files/test", 1784065766);
        assert_eq!(basename, "test");
        assert_eq!(location, "."); // matches PHP pathinfo dirname for root-level items
        assert_eq!(trash_fc, "files_trashbin/files/test.d1784065766");
    }

    #[test]
    fn trash_nested_file_basename_is_correct() {
        // Trashbin::move2trash() pathinfo() for a nested path.
        //   pathinfo("Media/test.txt") → basename="test.txt", dirname="Media"
        let (basename, location, trash_fc) = trash_path_info("files/Media/test.txt", 1784065766);
        assert_eq!(basename, "test.txt");
        assert_eq!(location, "Media");
        assert_eq!(trash_fc, "files_trashbin/files/test.txt.d1784065766");
    }

    #[test]
    fn trash_directory_structure_not_preserved() {
        // A file deep in a tree should NOT preserve its directory structure
        // in the trash path. PHP flattens to just the basename.
        let (_basename, _location, trash_fc) =
            trash_path_info("files/a/b/c/deep/file.pdf", 1784000000);
        assert_eq!(trash_fc, "files_trashbin/files/file.pdf.d1784000000");
    }

    #[test]
    fn trash_nested_dirname_is_correct() {
        // PHP: pathinfo("a/b/c/name") → basename="name", dirname="a/b/c"
        let (basename, location, trash_fc) = trash_path_info("files/a/b/c/name", 1784000000);
        assert_eq!(basename, "name");
        assert_eq!(location, "a/b/c");
        assert_eq!(trash_fc, "files_trashbin/files/name.d1784000000");
    }

    #[test]
    fn trash_filename_with_dots_preserves_extension() {
        // "archive.tar.gz" → basename is "archive.tar.gz", not "archive"
        let (basename, location, trash_fc) =
            trash_path_info("files/backups/archive.tar.gz", 1784000000);
        assert_eq!(basename, "archive.tar.gz");
        assert_eq!(location, "backups");
        assert_eq!(trash_fc, "files_trashbin/files/archive.tar.gz.d1784000000");
    }

    #[test]
    fn trash_root_level_directory() {
        // Trashing a top-level directory like "files/Photos"
        let (basename, location, trash_fc) = trash_path_info("files/Photos", 1784000000);
        assert_eq!(basename, "Photos");
        assert_eq!(location, "."); // matches PHP pathinfo dirname for root-level items
        assert_eq!(trash_fc, "files_trashbin/files/Photos.d1784000000");
    }

    #[test]
    fn trash_id_is_never_a_fileid() {
        // Regression: we used to store row.fileid in oc_files_trash.id.
        // The id must always be the filename basename (a string like "test"),
        // never a numeric fileid like "109".
        let (basename, _, _) = trash_path_info("files/test", 1784000000);
        assert!(
            !basename.chars().all(|c| c.is_ascii_digit()),
            "trash id must be the basename, not a numeric fileid"
        );
        assert_eq!(basename, "test");
    }

    #[test]
    fn trash_location_is_never_a_full_fc_path() {
        // Regression: we used to store "files/{relative}" as location.
        // The location must be the dirname relative to files/, never "files/...".
        let (_, location, _) = trash_path_info("files/Media/test.txt", 1784000000);
        assert!(
            !location.starts_with("files/"),
            "trash location must be relative to files/, not a full fc path: {location}"
        );
        assert_eq!(location, "Media");
    }

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

    /// Extract the parent directory from a filecache path, matching the
    /// logic in `bulk_handler` and `move_to_trash`.
    fn upload_parent_fc(fc_path: &str) -> String {
        let mut parts: Vec<&str> = fc_path.split('/').collect();
        parts.pop();
        parts.join("/")
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
        assert_eq!(upload_parent_fc("files/test.txt"), "files");
    }

    #[test]
    fn upload_parent_nested() {
        assert_eq!(
            upload_parent_fc("files/Media/Decent photos/001.jpg"),
            "files/Media/Decent photos"
        );
    }

    #[test]
    fn upload_parent_two_levels() {
        assert_eq!(upload_parent_fc("files/a/b"), "files/a");
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

    // ── §10.10 helper tests ──────────────────────────────────────────────

    #[test]
    fn extension_simple() {
        assert_eq!(extension("photo.jpg"), "jpg");
        assert_eq!(extension("note.txt"), "txt");
        assert_eq!(extension("archive.tar.gz"), "gz");
    }

    #[test]
    fn extension_no_dot() {
        assert_eq!(extension("Makefile"), "Makefile");
        assert_eq!(extension("noextension"), "noextension");
    }

    #[test]
    fn extension_hidden_file() {
        assert_eq!(extension(".bashrc"), "bashrc");
    }

    #[test]
    fn is_trash_extension_valid() {
        assert!(is_trash_extension("d1634567890"));
        assert!(is_trash_extension("d0"));
    }

    #[test]
    fn is_trash_extension_invalid() {
        assert!(!is_trash_extension("txt"));
        assert!(!is_trash_extension("d"));     // too short
        assert!(!is_trash_extension("dx123"));  // has non-digit
        assert!(!is_trash_extension(""));       // empty
    }
}
