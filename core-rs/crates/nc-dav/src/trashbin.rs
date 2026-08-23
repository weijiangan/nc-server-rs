//! `files_trashbin` — the delete path.
//!
//! Every DELETE routes through [`NcFileSystem::delete_file`] /
//! [`NcFileSystem::delete_dir`], which mirror PHP's `View::unlink()` /
//! `View::rmdir()`: capture the size, trash-or-hard-delete, then propagate.
//! The trash move itself replicates `Trashbin::move2trash()` plus the
//! `retainVersions()` / delete-hook cascade.

use sqlx::Row;
use tracing::warn;

use dav_server::fs::FsError;

use crate::cache_rows::ensure_lazy_dir_row;
use crate::filesystem::{blocking, io_to_fs};
use crate::row;
use crate::NcFileSystem;
use nc_db::{db_dispatch, db_execute};

impl NcFileSystem {
    /// Apply PHP's `Storage::doDelete` trashbin eligibility gates.
    pub(crate) async fn should_move_to_trash(&self, fc_path: &str) -> bool {
        if !self.is_trashbin_app_enabled().await {
            return false;
        }
        if fc_path.ends_with(".part") {
            return false;
        }
        if self.skip_trashbin {
            return false;
        }
        fc_path.starts_with("files/")
    }

    /// Check whether the `files_trashbin` app is enabled for this user.
    async fn is_trashbin_app_enabled(&self) -> bool {
        let sql = format!(
            "SELECT configvalue FROM {prefix}appconfig \
             WHERE appid = 'files_trashbin' AND configkey = 'enabled'",
            prefix = self.state.table_prefix
        );
        let enabled = db_dispatch!(&self.state.pool, |Db, c| {
            sqlx::query_scalar::<Db, String>(&sql)
                .fetch_optional(c)
                .await
        });
        match enabled {
            Ok(Some(value)) => value == "yes" || value == "true",
            Ok(None) => {
                let cache = self
                    .state
                    .appconfig_cache
                    .read()
                    .expect("appconfig cache lock");
                cache
                    .get_string("files_trashbin", "enabled")
                    .is_some_and(|value| value == "yes" || value == "true")
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

        let trash_fc = if self.should_move_to_trash(fc_path).await {
            Some(self.trash_directory(fc_path).await?)
        } else {
            None
        };
        if trash_fc.is_none() {
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
            let result = db_execute!(
                &self.state.pool,
                &sql_ext,
                self.storage_id,
                fc_path,
                &like_pat
            );
            if let Err(e) = result {
                tracing::warn!(fc_path = fc_path, error = %e, "Failed to delete filecache_extended rows for directory");
            }

            let sql_subtree = format!(
                "DELETE FROM {prefix}filecache WHERE storage = $1 AND (path = $2 OR path LIKE $3)"
            );
            db_dispatch!(&self.state.pool, |Db, c| {
                sqlx::query::<Db>(&sql_subtree)
                    .bind(self.storage_id)
                    .bind(fc_path)
                    .bind(&like_pat)
                    .execute(c)
                    .await
                    .map(|_| ())
                    .map_err(|_| FsError::GeneralFailure)?
            })
        }

        // §9.2: always propagate — matching PHP's View::rmdir() calling
        // Updater::remove() regardless of whether the storage trashed or
        // hard-deleted.  The trash-chain propagation runs first and the
        // source chain last (PHP's Updater::remove is the final root etag
        // writer — see delete_file for the full ordering rationale).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if let Some(tfc) = trash_fc.as_deref() {
            self.propagate_trash_target(fc_path, tfc, now).await;
        }
        // PHP `retainVersions` runs after the trash-chain propagation — its
        // version-move stamps are the final `files_trashbin` etag writers
        // (see delete_file for the full rationale).
        if trash_fc.is_some() {
            let relative = fc_path.strip_prefix("files/").unwrap_or(fc_path);
            self.trash_versions(relative, now, dir_row.fileid).await;
        }
        if deleted_size > 0 {
            if let Err(e) = self
                .propagator
                .propagate_change(fc_path, now, -deleted_size)
                .await
            {
                tracing::warn!(path = %fc_path, error = %e, "delete_dir: propagation failed (size>0)");
            }
        } else {
            if let Err(e) = self.propagator.propagate_change(fc_path, now, 0).await {
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

        let trash_fc = if self.should_move_to_trash(fc_path).await {
            Some(self.move_to_trash(fc_path, &frow).await?)
        } else {
            None
        };
        if trash_fc.is_none() {
            let disk = self.disk_path(fc_path);
            let d = disk.clone();
            blocking(move || std::fs::remove_file(&d))
                .await
                .map_err(io_to_fs)?;

            let sql = format!(
                "DELETE FROM {prefix}filecache WHERE fileid = $1",
                prefix = self.state.table_prefix
            );
            db_dispatch!(&self.state.pool, |Db, c| {
                sqlx::query::<Db>(&sql)
                    .bind(frow.fileid)
                    .execute(c)
                    .await
                    .map(|_| ())
                    .map_err(|_| FsError::GeneralFailure)?
            });

            let sql_ext = format!(
                "DELETE FROM {prefix}filecache_extended WHERE fileid = $1",
                prefix = self.state.table_prefix
            );
            let result = db_execute!(&self.state.pool, &sql_ext, frow.fileid);
            if let Err(e) = result {
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

        // PHP `renameFromStorage` target-chain side effects
        // (`copyOrRenameFromStorage`, Updater.php:192-204): the trash ancestors
        // get their sizes recomputed, both the source and the target direct
        // parents get their `storage_mtime` corrected, and the target chain
        // gets etag/mtime propagated.  Runs BEFORE the source-chain
        // propagation below.
        if let Some(tfc) = trash_fc.as_deref() {
            self.propagate_trash_target(fc_path, tfc, now).await;
        }

        // PHP `Trashbin::retainVersions()` — the version-file moves + the
        // `oc_files_versions` cleanup — runs AFTER the trash-chain propagation
        // (PHP runs it after renameFromStorage, Trashbin.php:355): each moved
        // version's renameFromStorage stamps `files_trashbin` again, so the
        // version-move stamps are the final `files_trashbin` etag writers
        // (live-verified: the oracle ends with
        // `files_trashbin.etag == files_trashbin/versions.etag`).
        if trash_fc.is_some() {
            let relative = fc_path.strip_prefix("files/").unwrap_or(fc_path);
            self.trash_versions(relative, now, frow.fileid).await;
        }

        // Source-chain propagation — deliberately LAST.  In PHP the source
        // chain is stamped twice: `renameFromStorage`'s propagateChange and
        // then `View::unlink`'s post-op `Updater::remove` (View.php:1247-1248,
        // Updater.php:102-115), which runs after the trash move and is the
        // final writer of the storage root's etag.  Live-verified oracle state
        // after a delete: `root.etag == files/etag` (one shared value) while
        // `files_trashbin.etag == files_trashbin/files.etag` (the trash
        // chain's stamp).  If the source chain ran first, the trash chain
        // would win on root and `root != files`.
        if deleted_size > 0 {
            if let Err(e) = self
                .propagator
                .propagate_change(fc_path, now, -deleted_size)
                .await
            {
                tracing::warn!(path = %fc_path, error = %e, "delete_file: propagation failed (size>0)");
            }
        } else {
            if let Err(e) = self.propagator.propagate_change(fc_path, now, 0).await {
                tracing::warn!(path = %fc_path, error = %e, "delete_file: propagation failed (size=0)");
            }
        }

        Ok(())
    }

    // ── trash_directory ───────────────────────────────────────────────────

    /// Move a directory tree to the trashbin.  Returns the trash `fc_path`.
    ///
    /// Does NOT handle propagation — [`delete_dir`] does that centrally
    /// (matching PHP's View → Updater separation).
    pub(crate) async fn trash_directory(&self, fc_path: &str) -> Result<String, FsError> {
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
        let descendants: Vec<(i64, String)> = db_dispatch!(&self.state.pool, |Db, c| {
            sqlx::query::<Db>(&sql_desc)
                .bind(self.storage_id)
                .bind(&like_pat)
                .fetch_all(c)
                .await
                .map_err(|_| FsError::GeneralFailure)?
                .into_iter()
                .map(|r| (r.get::<i64, _>("fileid"), r.get::<String, _>("path")))
                .collect()
        });

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
            let result = db_execute!(&self.state.pool, &sql_upd, &new_path, &new_hash, fid);
            if let Err(e) = result {
                tracing::warn!(fileid = fid, error = %e, "Failed to update descendant path in trash");
            }
        }

        Ok(trash_fc)
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

        // PHP `setUpTrash()` (Trashbin.php:162-176) creates all four trashbin
        // skeleton dirs on first use: `files_trashbin`, `files_trashbin/files`,
        // `files_trashbin/versions`, `files_trashbin/keys`.  The parent chain
        // above covers the first two; create the other two the same way
        // (idempotent — no-op when they already exist).
        for skeleton in ["files_trashbin/versions", "files_trashbin/keys"] {
            if let Err(e) = self.ensure_parent_dir(skeleton).await {
                warn!("move_to_trash: ensure {skeleton} failed: {e}");
            }
        }

        // PHP lazily materializes the user's `cache/` filecache row on the
        // first files access (the delete flow, or the first PUT/PROPFIND on a
        // fresh instance — live-verified; finding #8).  The triggering read is
        // not identified in the PHP source; replicate the observable row
        // create-if-missing (scanner-insert shape: size 0, permissions 31, no
        // extended row).
        ensure_lazy_dir_row(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            &self.state.mime_cache,
            "cache",
            now,
        )
        .await;

        // PHP `Trashbin::retainVersions()` (Trashbin.php:391-417) — the
        // version-file moves + `oc_files_versions` cleanup — runs in the
        // CALLERS after the trash-chain propagation (PHP runs it after
        // renameFromStorage, so its stamps are the final `files_trashbin`
        // etag writers; see delete_file/delete_dir).

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

        // Update the filecache row.  Mirrors PHP `Cache::move` (Cache.php:813-831):
        // path/path_hash/name/parent only — **mtime is left untouched** (the
        // trashed file keeps the mtime it had before deletion; live-verified).
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
             SET path=$1, path_hash=$2, name=$3, parent=$4 \
             WHERE fileid=$5",
            prefix = self.state.table_prefix
        );
        db_dispatch!(&self.state.pool, |Db, c| {
            sqlx::query::<Db>(&sql)
                .bind(&trash_fc)
                .bind(&new_hash)
                .bind(&new_name)
                .bind(trash_parent.fileid)
                .bind(row.fileid)
                .execute(c)
                .await
                .map(|_| ())
                .map_err(|_| FsError::GeneralFailure)?
        });

        // PHP `updateStorageMTimeOnly($target)` (Updater.php:207-220): the
        // moved file's own `storage_mtime` ← its disk mtime (mtime untouched).
        let disk_mtime = std::fs::metadata(&to_disk)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(now);
        let sql_sm = format!(
            "UPDATE {prefix}filecache SET storage_mtime = $1 WHERE fileid = $2",
            prefix = self.state.table_prefix
        );
        let result = db_execute!(&self.state.pool, &sql_sm, disk_mtime, row.fileid);
        if let Err(e) = result {
            tracing::warn!(fileid = row.fileid, error = %e, "move_to_trash: storage_mtime update failed");
        }

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
        let result = db_execute!(
            &self.state.pool,
            &trash_sql,
            &trash_basename,
            &self.uid,
            now.to_string(),
            &trash_location,
            &self.uid
        );
        if let Err(e) = result {
            // Non-fatal: the file is already trashed on disk and in the
            // filecache, so it still lists in the web UI. But without this row
            // the original location is lost and PHP restore falls back to root.
            warn!("oc_files_trash insert failed for {trash_basename}: {e}");
        }

        Ok(trash_fc)
    }

    /// Mirror the target-chain half of PHP `renameFromStorage`
    /// (`copyOrRenameFromStorage`, Updater.php:192-204) for a move-to-trash:
    /// the trash ancestors' sizes get recomputed from their children, both
    /// the source and the target direct parents get their `storage_mtime`
    /// corrected to their disk mtimes, and the target chain gets etag/mtime
    /// propagated (findings #6/#12).
    ///
    /// The source chain's size subtraction is done by the caller's existing
    /// `propagate_change(fc_path, now, -size)`; this fills in the trash side.
    async fn propagate_trash_target(&self, source_fc: &str, trash_fc: &str, now: i64) {
        // correctFolderSize(target) — recompute trash ancestors from children.
        if let Err(e) = self.propagator.correct_folder_size_chain(trash_fc).await {
            tracing::warn!(path = %trash_fc, error = %e, "trash: trash-chain folder-size recompute failed");
        }
        // correctParentStorageMtime(source) + correctParentStorageMtime(target).
        for (path, label) in [
            (source_fc.to_string(), "source"),
            (trash_fc.to_string(), "target"),
        ] {
            let parent_fc = path
                .rsplit_once('/')
                .map(|(p, _)| p.to_string())
                .unwrap_or_default();
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
                    "trash: parent storage_mtime correction failed"
                );
            }
        }
        // propagateChange(target, time, 0) — etag/mtime only, size already recomputed.
        if let Err(e) = self.propagator.propagate_change(trash_fc, now, 0).await {
            tracing::warn!(path = %trash_fc, error = %e, "trash: trash-chain etag/mtime propagation failed");
        }
    }

    /// Mirror PHP `Trashbin::retainVersions()` (Trashbin.php:391-417) plus the
    /// `oc_files_versions` cleanup of the delete-hook cascade
    /// (`FileEventsListener::remove_hook` → `deleteVersionsEntity` →
    /// `deleteAllVersionsForFileId`, LegacyVersionsBackend.php:281-283 +
    /// VersionsMapper.php:63-69): move the trashed file's versions from
    /// `files_versions/` to `files_trashbin/versions/` (so the web UI can
    /// restore them) and delete its `oc_files_versions` metadata rows
    /// (finding #10).
    ///
    /// The row DELETE is **unconditional** — live-verified: PHP removes the
    /// version-entity rows even when no version *file* exists on disk (the
    /// SUT's PUT creates a row for every write; without an unconditional
    /// cleanup the row would survive the delete and diverge).  It matches by
    /// the trashed node's OWN file id (`deleteAllVersionsForFileId`) — so a
    /// directory trash deletes nothing, since only the deleted node's id is
    /// used and directories never have version rows.
    ///
    /// The version-file move + `files_versions`/`files_trashbin/versions`
    /// size recomputes stay gated on version files existing (PHP moves them
    /// only when present).
    ///
    /// Must run **before** the file's own filecache row is re-keyed to the
    /// trash path.
    async fn trash_versions(&self, relative: &str, now: i64, fileid: i64) {
        let pool = &self.state.pool;
        let prefix = &self.state.table_prefix;

        // Unconditional row cleanup — matches PHP `deleteAllVersionsForFileId`
        // (DELETE FROM oc_files_versions WHERE file_id = ?), which runs even
        // when the file has no version files on disk.
        let sql_del = format!(
            "DELETE FROM {prefix}files_versions WHERE file_id = $1",
            prefix = prefix
        );
        let result = db_execute!(pool, &sql_del, fileid);
        if let Err(e) = result {
            warn!(fileid = fileid, error = %e, "trash_versions: oc_files_versions DELETE failed");
        }

        let versions_base = format!("files_versions/{relative}");
        let base_like = format!("{versions_base}/%");
        let file_like = format!("{versions_base}.v%");

        let sql = format!(
            "SELECT fileid, path FROM {prefix}filecache \
             WHERE storage = $1 AND (path = $2 OR path LIKE $3 OR path LIKE $4) \
             ORDER BY path",
            prefix = prefix
        );
        let rows: Vec<(i64, String)> = db_dispatch!(pool, |Db, c| {
            match sqlx::query::<Db>(&sql)
                .bind(self.storage_id)
                .bind(&versions_base)
                .bind(&base_like)
                .bind(&file_like)
                .fetch_all(c)
                .await
            {
                Ok(rs) => rs
                    .into_iter()
                    .map(|r| (r.get::<i64, _>("fileid"), r.get::<String, _>("path")))
                    .collect(),
                Err(e) => {
                    warn!(error = %e, "trash_versions: version-row query failed");
                    return;
                }
            }
        });
        if rows.is_empty() {
            return;
        }

        let trash_versions_parent =
            match row::lookup_by_path(pool, prefix, self.storage_id, "files_trashbin/versions")
                .await
            {
                Some(r) => r,
                None => {
                    warn!("trash_versions: files_trashbin/versions missing from filecache");
                    return;
                }
            };

        let versions_len = versions_base.len();
        let mut first_target: Option<String> = None;
        for (fid, old_path) in &rows {
            let is_subtree_child =
                old_path.len() > versions_len && old_path[versions_len..].starts_with('/');
            let target = if is_subtree_child {
                // Directory subtree child: keep the relative structure under
                // files_trashbin/versions/{basename}.d{now}.  (Bare
                // `{basename}.d{now}` — `trash_fc_name` would add the
                // `files_trashbin/files/` prefix, which belongs to the main
                // trash location only.)
                let base = relative.rsplit('/').next().unwrap_or(relative);
                format!(
                    "files_trashbin/versions/{base}.d{now}{}",
                    &old_path[versions_len..]
                )
            } else {
                // The subtree root (directory case) or a `.v{ts}` sibling
                // (file case): PHP getTrashFilename() appends `.d{now}` to the
                // existing name.
                let base = old_path.rsplit('/').next().unwrap_or(old_path);
                format!("files_trashbin/versions/{base}.d{now}")
            };
            if first_target.is_none() {
                first_target = Some(target.clone());
            }

            if is_subtree_child {
                // The subtree root's disk rename carried the whole directory —
                // children only get their filecache paths re-keyed (PHP
                // `moveFromCache` dir branch, Cache.php:749-768).  Parents stay
                // the same fileids (they moved with the subtree).
                let new_hash = row::path_hash(&target);
                let sql_upd = format!(
                    "UPDATE {prefix}filecache SET path=$1, path_hash=$2 WHERE fileid=$3",
                    prefix = prefix
                );
                let result = db_execute!(pool, &sql_upd, &target, &new_hash, fid);
                if let Err(e) = result {
                    warn!(fileid = fid, error = %e, "trash_versions: child path update failed");
                }
            } else {
                // Move on disk (the subtree root or a `.v{ts}` sibling).
                let from_disk = self.disk_path(old_path);
                let to_disk = self.disk_path(&target);
                if let Some(parent) = to_disk.parent() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        warn!("trash_versions: mkdir {} failed: {e}", parent.display());
                        continue;
                    }
                }
                let f = from_disk.clone();
                let t = to_disk.clone();
                if let Err(e) = blocking(move || std::fs::rename(&f, &t)).await {
                    warn!("trash_versions: rename {old_path} → {target} failed: {e}");
                    continue;
                }

                // Re-key the row: new name + parent (PHP `moveFromCache` single
                // row branch, Cache.php:809-831).
                let new_hash = row::path_hash(&target);
                let new_name = target.rsplit('/').next().unwrap_or(&target).to_string();
                let sql_upd = format!(
                    "UPDATE {prefix}filecache \
                     SET path=$1, path_hash=$2, name=$3, parent=$4 \
                     WHERE fileid=$5",
                    prefix = prefix
                );
                let result = db_execute!(
                    pool,
                    &sql_upd,
                    &target,
                    &new_hash,
                    &new_name,
                    trash_versions_parent.fileid,
                    fid
                );
                if let Err(e) = result {
                    warn!(fileid = fid, error = %e, "trash_versions: row update failed");
                }
            }

            // PHP `retainVersions` → `move()` → `renameFromStorage`
            // (Trashbin.php:445-459, Updater.php:203-204) propagates etag/mtime
            // on the version's source chain (`files_versions/…`) and its target
            // chain (`files_trashbin/versions`) — the last move's target-chain
            // stamp wins on `files_trashbin` (live-verified: the oracle ends
            // with `files_trashbin.etag == files_trashbin/versions.etag`).
            if let Err(e) = self.propagator.propagate_change(old_path, now, 0).await {
                tracing::warn!(path = %old_path, error = %e, "trash_versions: source-chain propagation failed");
            }
            if let Err(e) = self.propagator.propagate_change(&target, now, 0).await {
                tracing::warn!(path = %target, error = %e, "trash_versions: target-chain propagation failed");
            }
        }

        // PHP's renameFromStorage recomputes sizes on both the source chain
        // (`files_versions/…`) and the target chain (`files_trashbin/versions`).
        if let Some(old_first) = rows.first().map(|(_, p)| p.clone()) {
            if let Err(e) = self.propagator.correct_folder_size_chain(&old_first).await {
                tracing::warn!(path = %old_first, error = %e, "trash_versions: source-chain size recompute failed");
            }
        }
        if let Some(t) = &first_target {
            if let Err(e) = self.propagator.correct_folder_size_chain(t).await {
                tracing::warn!(path = %t, error = %e, "trash_versions: target-chain size recompute failed");
            }
        }
    }
}

/// Build the PHP-compatible trash filecache path for a basename and timestamp.
pub(crate) fn trash_fc_name(basename: &str, timestamp: i64) -> String {
    format!("files_trashbin/files/{basename}.d{timestamp}")
}

#[cfg(test)]
mod tests {
    use sqlx::Sqlite;

    use crate::row;
    use crate::testing::{
        etag_of, extended_count, fc_row, fresh_data_dir, fresh_delete_db, test_fs, test_pool,
        touch, trash_ts,
    };

    use super::trash_fc_name;

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

    /// The trashed file keeps its original mtime and gets `storage_mtime` from
    /// its disk mtime (PHP `Cache::move` + `updateStorageMTimeOnly`); the trash
    /// skeleton (`keys`/`versions`) is created (findings #7/#12 follow-ups).
    #[tokio::test]
    async fn move_to_trash_preserves_mtime_and_creates_skeleton() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/hello.txt"));
        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir.clone());

        let row = row::lookup_by_path(&pool, &prefix, storage_id, "files/hello.txt")
            .await
            .unwrap();
        fs.move_to_trash("files/hello.txt", &row).await.unwrap();

        // The row is gone from its original path; find it under files_trashbin.
        assert!(fc_row(&pool, &prefix, "files/hello.txt").await.is_none());
        let ts = trash_ts(&pool).await;
        let expected = format!("files_trashbin/files/hello.txt.d{ts}");
        let (_, size, mtime, storage_mtime, _, trash_path) =
            fc_row(&pool, &prefix, &expected).await.unwrap();
        assert_eq!(size, 26);
        assert_eq!(mtime, 100, "trashed file must keep its original mtime");
        assert_eq!(trash_path, expected);
        let disk_mtime = std::fs::metadata(data_dir.join("admin").join(&trash_path))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap();
        assert!(
            (storage_mtime - disk_mtime).abs() <= 1,
            "storage_mtime {storage_mtime} should be the disk mtime {disk_mtime}"
        );

        // Skeleton dirs created (PHP setUpTrash) — including keys + versions.
        for p in [
            "files_trashbin",
            "files_trashbin/files",
            "files_trashbin/versions",
            "files_trashbin/keys",
        ] {
            assert!(
                row::lookup_by_path(&pool, &prefix, storage_id, p)
                    .await
                    .is_some(),
                "missing skeleton dir {p}"
            );
        }

        // oc_files_trash row inserted.
        let trash_count: i64 =
            sqlx::query_scalar::<Sqlite, _>("SELECT COUNT(*) FROM oc_files_trash")
                .fetch_one(test_pool(&pool))
                .await
                .unwrap();
        assert_eq!(trash_count, 1);
    }

    /// Finding #6: the trash ancestors get the trashed file's size, the source
    /// ancestors lose it, and the storage_mtime is corrected on both parents
    /// (PHP `copyOrRenameFromStorage` target-chain half).
    #[tokio::test]
    async fn delete_file_trash_propagates_sizes_and_storage_mtime() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/hello.txt"));
        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir.clone());

        fs.delete_file("files/hello.txt").await.unwrap();

        // Source chain: files/ loses the 26 bytes.
        let (_, size, _, storage_mtime, _, _) = fc_row(&pool, &prefix, "files").await.unwrap();
        assert_eq!(size, 74, "files/ must lose the trashed file's size");
        let files_disk_mtime = std::fs::metadata(data_dir.join("admin/files"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap();
        assert!(
            (storage_mtime - files_disk_mtime).abs() <= 1,
            "files/ storage_mtime {storage_mtime} should be its disk mtime {files_disk_mtime}"
        );

        // Target chain: trash ancestors gain the 26 bytes.
        let (_, size, _, _, _, _) = fc_row(&pool, &prefix, "files_trashbin/files")
            .await
            .unwrap();
        assert_eq!(
            size, 26,
            "files_trashbin/files must gain the trashed file's size"
        );
        let (_, size, _, _, _, _) = fc_row(&pool, &prefix, "files_trashbin").await.unwrap();
        assert_eq!(size, 26, "files_trashbin must gain the trashed file's size");

        // Root recomputed from children: files(74) + files_trashbin(26) + files_versions(0).
        let (_, size, _, _, _, _) = fc_row(&pool, &prefix, "").await.unwrap();
        assert_eq!(size, 100);
    }

    /// Finding #10: versions move to `files_trashbin/versions/` and the
    /// `oc_files_versions` metadata row is deleted (PHP `retainVersions` +
    /// `remove_hook`).
    #[tokio::test]
    async fn trash_versions_moves_version_file_and_deletes_rows() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/hello.txt"));
        touch(&data_dir.join("admin/files_versions/hello.txt.v100"));

        // Version filecache row (id 5) under files_versions + oc_files_versions row.
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_filecache \
             (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
              size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES (5, 1, 'files_versions/hello.txt.v100', $1, 6, 'hello.txt.v100', 0, 0, 26, 100, 100, 'etag', 27, '')",
        )
        .bind(row::path_hash("files_versions/hello.txt.v100"))
        .execute(test_pool(&pool))
        .await
        .unwrap();
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_files_versions (id, file_id, \"timestamp\", size, mimetype, metadata) \
             VALUES (1, 4, 100, 26, 0, '{\"author\":\"admin\"}')",
        )
        .execute(test_pool(&pool))
        .await
        .unwrap();

        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir.clone());
        // The version moves now run in the delete flow (PHP `retainVersions`
        // runs after the trash move) — exercise the full delete path.
        fs.delete_file("files/hello.txt").await.unwrap();

        // Version file moved on disk and re-keyed under files_trashbin/versions.
        let ts = trash_ts(&pool).await;
        let expected_v = format!("files_trashbin/versions/hello.txt.v100.d{ts}");
        let (_, size, mtime, _, _, trash_vpath) =
            fc_row(&pool, &prefix, &expected_v).await.unwrap();
        assert_eq!(size, 26);
        assert_eq!(mtime, 100);
        assert_eq!(trash_vpath, expected_v);
        assert!(
            data_dir.join("admin").join(&trash_vpath).exists(),
            "version file must exist at {trash_vpath}"
        );
        assert!(
            !data_dir
                .join("admin/files_versions/hello.txt.v100")
                .exists(),
            "version file must leave files_versions/"
        );

        // oc_files_versions row deleted (PHP remove_hook → deleteVersionsEntity).
        let vcount: i64 = sqlx::query_scalar::<Sqlite, _>("SELECT COUNT(*) FROM oc_files_versions")
            .fetch_one(test_pool(&pool))
            .await
            .unwrap();
        assert_eq!(vcount, 0);
    }

    /// Finding #10 for directories: the whole `files_versions/{dir}/` subtree
    /// moves.  The `oc_files_versions` cleanup matches by the TRASHED NODE's
    /// own file id (PHP `deleteAllVersionsForFileId` — the delete-hook
    /// cascade fires only for the deleted node, never for the files moved
    /// along inside it), so a directory trash leaves the inner files' version
    /// rows in place — the version files moved to `files_trashbin/versions/`
    /// keep their fileids, so the rows stay consistent.
    #[tokio::test]
    async fn trash_directory_moves_version_subtree_and_deletes_rows() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/dir/a.txt"));
        touch(&data_dir.join("admin/files_versions/dir/a.txt.v100"));

        // files/dir (id 7, size 8) + files/dir/a.txt (id 9) +
        // files_versions/dir (id 10) + files_versions/dir/a.txt.v100 (id 11).
        for (fid, path, parent, name) in [
            (7i64, "files/dir", 2i64, "dir"),
            (9, "files/dir/a.txt", 7, "a.txt"),
            (10, "files_versions/dir", 6, "dir"),
            (11, "files_versions/dir/a.txt.v100", 10, "a.txt.v100"),
        ] {
            sqlx::query::<Sqlite>(&format!(
                "INSERT INTO {prefix}filecache \
                 (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
                  size, mtime, storage_mtime, etag, permissions, checksum) \
                 VALUES ($1, 1, $2, $3, $4, $5, 0, 0, 8, 100, 100, 'etag', 27, '')"
            ))
            .bind(fid)
            .bind(path)
            .bind(row::path_hash(path))
            .bind(parent)
            .bind(name)
            .execute(test_pool(&pool))
            .await
            .unwrap();
        }
        // The seeded `files/hello.txt` row references parent 2 — leave it; the
        // subtree DELETE only touches files/dir rows.  Drop the hello.txt row
        // so it doesn't linger under the (now-trashed) files/ tree.
        sqlx::query::<Sqlite>("DELETE FROM oc_filecache WHERE fileid = 4")
            .execute(test_pool(&pool))
            .await
            .unwrap();
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_files_versions (id, file_id, \"timestamp\", size, mimetype, metadata) \
             VALUES (1, 9, 100, 8, 0, '{\"author\":\"admin\"}')",
        )
        .execute(test_pool(&pool))
        .await
        .unwrap();

        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir.clone());
        // The version-subtree moves now run in the delete flow (PHP
        // `retainVersions` runs after the trash move) — exercise the full
        // delete path.
        fs.delete_dir("files/dir").await.unwrap();
        let ts = trash_ts(&pool).await;

        // Subtree version file moved and re-keyed under files_trashbin/versions.
        let expected_v = format!("files_trashbin/versions/dir.d{ts}/a.txt.v100");
        let (_, _, _, _, _, vpath) = fc_row(&pool, &prefix, &expected_v).await.unwrap();
        assert_eq!(vpath, expected_v);
        assert!(data_dir.join("admin").join(&vpath).exists());

        // The inner file's oc_files_versions row SURVIVES: PHP's
        // deleteAllVersionsForFileId runs for the trashed dir's own id only
        // (dirs have no version rows), and the moved files' rows stay
        // consistent with their (unchanged) fileids.
        let vcount: i64 = sqlx::query_scalar::<Sqlite, _>("SELECT COUNT(*) FROM oc_files_versions")
            .fetch_one(test_pool(&pool))
            .await
            .unwrap();
        assert_eq!(
            vcount, 1,
            "dir trash must not delete inner files' version rows"
        );

        // The main file moved too (id 9 under files_trashbin/files/dir.d{ts}).
        assert!(fc_row(&pool, &prefix, "files/dir/a.txt").await.is_none());
        let expected_m = format!("files_trashbin/files/dir.d{ts}/a.txt");
        let (_, _, _, _, _, p9) = fc_row(&pool, &prefix, &expected_m).await.unwrap();
        assert_eq!(p9, expected_m);
    }

    /// Finding #11: the preview_generation row survives the trash move (it was
    /// queued by the PUT; the filecache row keeps its fileid through the trash).
    #[tokio::test]
    async fn trash_keeps_preview_generation_row() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/hello.txt"));
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_preview_generation (id, uid, file_id, queued_at) \
             VALUES (1, 'admin', 4, 100)",
        )
        .execute(test_pool(&pool))
        .await
        .unwrap();

        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir);
        fs.delete_file("files/hello.txt").await.unwrap();

        let pcount: i64 =
            sqlx::query_scalar::<Sqlite, _>("SELECT COUNT(*) FROM oc_preview_generation")
                .fetch_one(test_pool(&pool))
                .await
                .unwrap();
        assert_eq!(pcount, 1, "preview row must survive the trash move");
    }

    /// Finding #10: the `oc_files_versions` cleanup is unconditional — the row
    /// the SUT's PUT inserted is deleted even when no version FILE exists on
    /// disk (live-verified PHP behavior).
    #[tokio::test]
    async fn delete_file_deletes_version_row_without_version_files() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/hello.txt"));
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_files_versions (id, file_id, \"timestamp\", size, mimetype, metadata) \
             VALUES (1, 4, 100, 26, 0, '{\"author\":\"admin\"}')",
        )
        .execute(test_pool(&pool))
        .await
        .unwrap();

        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir);
        fs.delete_file("files/hello.txt").await.unwrap();

        let vcount: i64 = sqlx::query_scalar::<Sqlite, _>("SELECT COUNT(*) FROM oc_files_versions")
            .fetch_one(test_pool(&pool))
            .await
            .unwrap();
        assert_eq!(
            vcount, 0,
            "version rows must be deleted even without version files"
        );
    }

    /// Finding #12 (etag pattern): after a delete-to-trash the storage root
    /// and `files/` share ONE etag (the source chain, stamped last — PHP's
    /// `Updater::remove` runs after the trash move), while `files_trashbin`
    /// and `files_trashbin/files` share the trash chain's etag; `keys` and
    /// `versions` keep distinct mkdir/insert etags.
    #[tokio::test]
    async fn delete_file_etag_equality_pattern() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/hello.txt"));
        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir);

        fs.delete_file("files/hello.txt").await.unwrap();

        let root = etag_of(&pool, &prefix, "").await.unwrap();
        let files = etag_of(&pool, &prefix, "files").await.unwrap();
        let trash = etag_of(&pool, &prefix, "files_trashbin").await.unwrap();
        let trash_files = etag_of(&pool, &prefix, "files_trashbin/files")
            .await
            .unwrap();
        let keys = etag_of(&pool, &prefix, "files_trashbin/keys")
            .await
            .unwrap();
        let versions = etag_of(&pool, &prefix, "files_trashbin/versions")
            .await
            .unwrap();
        assert_eq!(
            root, files,
            "root and files/ must share the source-chain etag"
        );
        assert_eq!(
            trash, trash_files,
            "files_trashbin and files_trashbin/files must share the trash-chain etag"
        );
        assert_ne!(root, trash, "root etag must differ from the trash chain's");
        assert_ne!(keys, versions);
        assert_ne!(keys, trash);
        assert_ne!(versions, trash);
    }

    /// Finding #12 (storage_mtime): creating `files_trashbin` (a direct child
    /// of the storage root) stamps the root's `storage_mtime` with the root
    /// dir's disk mtime (PHP `View::mkdir` → `Updater::update` →
    /// `correctParentStorageMtime`).
    #[tokio::test]
    async fn delete_file_stamps_root_storage_mtime() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/hello.txt"));
        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir.clone());

        fs.delete_file("files/hello.txt").await.unwrap();

        let (_, _, _, root_sm, _, _) = fc_row(&pool, &prefix, "").await.unwrap();
        let disk_mtime = std::fs::metadata(data_dir.join("admin"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap();
        assert!(
            (root_sm - disk_mtime).abs() <= 1,
            "root storage_mtime {root_sm} should be the root dir's disk mtime {disk_mtime}"
        );
    }

    /// Finding #8: the delete flow materializes the user's `cache/` filecache
    /// row (live-verified oracle behavior — the row appears during the DELETE;
    /// scanner-insert shape: size 0, permissions 31, no extended row).
    #[tokio::test]
    async fn delete_flow_materializes_cache_row() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/hello.txt"));
        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir);

        fs.delete_file("files/hello.txt").await.unwrap();

        let (_, size, mtime, sm, parent, path) = fc_row(&pool, &prefix, "cache").await.unwrap();
        assert_eq!(path, "cache");
        assert_eq!(size, 0);
        assert_eq!(parent, 1, "cache must hang off the storage root");
        assert!(mtime > 0 && sm > 0);
        assert_eq!(
            extended_count(&pool).await,
            0,
            "cache row must have no extended row"
        );
    }
}
