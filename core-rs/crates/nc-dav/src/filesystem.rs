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

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
use nc_db::mime::SharedMimeCache;
use nc_db::pool::DbPool;

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
    /// Per-request batched PROPFIND data (Phase 18.1).  `read_dir` fetches
    /// everything `get_props` needs for every child in a handful of batched
    /// queries and stores it here; `get_props` reads the cache instead of
    /// re-querying per node.  Nodes outside the batch (the depth-0 root,
    /// which dav-server-rs visits before `read_dir`) fall back to the
    /// single-row queries.
    pub(crate) propfind_batch: PropfindBatch,
}

/// Read a value from a per-request batch map, or `None` when the node is
/// outside the batch (caller falls back to the single-row query).
fn batch_get<K, V, Q>(m: &Arc<Mutex<HashMap<K, V>>>, k: &Q) -> Option<V>
where
    K: Eq + std::hash::Hash + std::borrow::Borrow<Q> + Clone,
    Q: Eq + std::hash::Hash + ?Sized,
    V: Clone,
{
    m.lock().expect("propfind batch lock").get(k).cloned()
}

/// Is `k` part of the `read_dir` batch?  If yes, a map miss means "no data"
/// and `get_props` must NOT fall back to a single-row query; if no, the node
/// is outside the batch (depth-0 root) and the single-row query is the only
/// source.
fn batch_contains<K, Q>(m: &Arc<Mutex<std::collections::HashSet<K>>>, k: &Q) -> bool
where
    K: Eq + std::hash::Hash + std::borrow::Borrow<Q>,
    Q: Eq + std::hash::Hash + ?Sized,
{
    m.lock().expect("propfind batch lock").contains(k)
}

/// Per-request cache of everything depth-1 PROPFIND needs per child.
///
/// All maps are `Arc<Mutex<…>>` because dav-server-rs clones the filesystem
/// per resource (`PropWriter`), and the clones must share one cache — a
/// plain `Mutex<HashMap>` would be snapshotted at every clone and the batch
/// would never reach the consumers.
///
/// Populated only by `read_dir` (a pure read path) and the per-request
/// uid-only lookups in `get_props`; write requests never run `read_dir`, so
/// no stale-row risk exists within a request.
#[derive(Clone, Default)]
pub(crate) struct PropfindBatch {
    /// The fileids `read_dir` batched.  `get_props` uses this to distinguish
    /// "child with no data" (in the set → map miss means empty, no query)
    /// from "node outside the batch" (not in the set → single-row query).
    pub(crate) children: Arc<Mutex<std::collections::HashSet<i64>>>,
    /// The fc paths `read_dir` batched (same role as `children`, for the
    /// path-keyed `oc_properties` lookup).
    pub(crate) child_paths: Arc<Mutex<std::collections::HashSet<String>>>,
    /// `fc_path` → metadata, keyed trailing-slash-normalized.  Serves
    /// `load_meta` so `get_props` never re-fetches a row `read_dir` holds.
    pub(crate) meta: Arc<Mutex<HashMap<String, NcMetaData>>>,
    /// Resolved once per request: the `oc_users`/`oc_accounts` display name
    /// of `uid` (`{oc:}owner-display-name`).
    pub(crate) display_name: Arc<Mutex<Option<String>>>,
    /// Resolved once per request: `shareapi_exclude_groups` state for `uid`.
    pub(crate) sharing_disabled: Arc<Mutex<Option<bool>>>,
    /// fileid → (dir_count, file_count) for `{nc:}contained-*-count`.
    pub(crate) dir_counts: Arc<Mutex<HashMap<i64, (i64, i64)>>>,
    /// fileid → share rows for `{oc:}share-types` / `{nc:}sharees`.
    pub(crate) share_details: Arc<Mutex<HashMap<i64, Vec<row::ShareDetail>>>>,
    /// fileid → most-recent non-empty share note.
    pub(crate) share_notes: Arc<Mutex<HashMap<i64, String>>>,
    /// fileid → (count, unread) for `{oc:}comments-*`.
    pub(crate) comments: Arc<Mutex<HashMap<i64, (i64, i64)>>>,
    /// fileid → system tags for `{nc:}system-tags`.
    pub(crate) system_tags: Arc<Mutex<HashMap<i64, Vec<row::SystemTagRow>>>>,
    /// raw `fc_path` → custom properties from `oc_properties`.
    pub(crate) custom_props: Arc<Mutex<HashMap<String, Vec<(String, String, i16)>>>>,
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
            propfind_batch: PropfindBatch::default(),
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
            let parent_fc_path = {
                let mut parts: Vec<&str> = built.split('/').collect();
                parts.pop();
                parts.join("/")
            };
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
        sqlx::query(&sql)
            .bind(&trash_fc)
            .bind(&new_hash)
            .bind(&new_name)
            .bind(trash_parent.fileid)
            .bind(row.fileid)
            .execute(&self.state.pool)
            .await
            .map_err(|_| FsError::GeneralFailure)?;

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
        if let Err(e) = sqlx::query(&sql_sm)
            .bind(disk_mtime)
            .bind(row.fileid)
            .execute(&self.state.pool)
            .await
        {
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
        if let Err(e) = self
            .propagator
            .propagate_change(trash_fc, now, 0)
            .await
        {
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
        if let Err(e) = sqlx::query(&sql_del).bind(fileid).execute(pool).await {
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
        let rows: Vec<(i64, String)> = match sqlx::query(&sql)
            .bind(self.storage_id)
            .bind(&versions_base)
            .bind(&base_like)
            .bind(&file_like)
            .fetch_all(pool)
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
        };
        if rows.is_empty() {
            return;
        }

        let trash_versions_parent = match row::lookup_by_path(
            pool,
            prefix,
            self.storage_id,
            "files_trashbin/versions",
        )
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
                if let Err(e) = sqlx::query(&sql_upd)
                    .bind(&target)
                    .bind(&new_hash)
                    .bind(fid)
                    .execute(pool)
                    .await
                {
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
                if let Err(e) = sqlx::query(&sql_upd)
                    .bind(&target)
                    .bind(&new_hash)
                    .bind(&new_name)
                    .bind(trash_versions_parent.fileid)
                    .bind(fid)
                    .execute(pool)
                    .await
                {
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

    /// Load `NcMetaData` for any filecache path, including extended times.
    async fn load_meta(&self, fc_path: &str) -> Option<NcMetaData> {
        // Phase 18.1: serve from the per-request batch populated by `read_dir`
        // — depth-1 PROPFIND's per-child lookups reuse the rows already in
        // hand instead of re-issuing two queries per node.  Round-3 Task 10:
        // additionally store on miss, so the root's double lookup per
        // PROPFIND (`fs.metadata(root)` then `get_props(root)` →
        // `load_meta(root)`) pays once.  All callers are read-only —
        // `metadata()`, the `open()` read path, and `get_props`; the write
        // paths call `lookup_by_path` directly and never consult the batch,
        // so an entry cannot go stale within a request.
        let key = fc_path.trim_end_matches('/');
        if let Some(meta) = batch_get(&self.propfind_batch.meta, key) {
            return Some(meta);
        }
        let found = row::lookup_by_path_with_ext(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            fc_path,
        )
        .await;
        tracing::trace!(
            fc_path = %fc_path,
            storage_id = self.storage_id,
            found = found.is_some(),
            "load_meta result"
        );
        let (row, ext) = found?;

        let mime_type = {
            let cache = self.state.mime_cache.read().expect("mime cache lock");
            cache
                .get_name(row.mimetype)
                .unwrap_or("application/octet-stream")
                .to_string()
        };

        let mut meta = NcMetaData::from_row(&row, mime_type, ext.metadata_etag.clone());
        meta.apply_extended(ext.creation_time, ext.upload_time, ext.metadata_etag);
        self.propfind_batch
            .meta
            .lock()
            .expect("propfind batch lock")
            .insert(key.to_string(), meta.clone());
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

            // Resolve the directory itself to get its fileid.  The request
            // root is usually already in the batch (load_meta store-on-miss
            // from `fs.metadata`/`get_props`), so reuse it instead of a
            // second lookup (round-3 Task 10).
            let dir_fileid = match batch_get(&self.propfind_batch.meta, fc_path.trim_end_matches('/'))
            {
                Some(meta) => meta.fileid,
                None => row::lookup_by_path(
                    &self.state.pool,
                    &self.state.table_prefix,
                    self.storage_id,
                    &fc_path,
                )
                .await
                .ok_or(FsError::NotFound)?
                .fileid,
            };

            // Fetch all direct children with their extended metadata in one
            // LEFT JOIN (round-3 Task 9) — the same single-query shape as
            // PHP's `Cache::getFolderContentsById` (`selectFileCache` +
            // `selectMetadata`, Cache.php:214).  Previously two queries
            // (list_children + list_extended_batch); children without an
            // extended row get zero times, as before.
            let (children, extended_map) = row::list_children_with_ext(
                &self.state.pool,
                &self.state.table_prefix,
                dir_fileid,
                self.storage_id,
            )
            .await;

            let child_fileids: Vec<i64> = children.iter().map(|c| c.fileid).collect();

            // §9.5: prefetch tags for the directory + all children so that
            // depth-1 PROPFIND has {oc:}favorite and {oc:}tags ready without
            // N+1 DB queries.  Include the directory itself.
            let mut prefetch_ids = child_fileids.clone();
            prefetch_ids.push(dir_fileid);
            crate::tags::prefetch_tags(
                &self.state.pool,
                &self.state.table_prefix,
                &self.uid,
                &prefetch_ids,
                &self.tag_cache,
            )
            .await;

            // ── Phase 18.1: per-request batch ────────────────────────────────
            // Build every child's metadata once, then populate the per-request
            // `propfind_batch` so per-child `get_props` reads cached values
            // instead of re-issuing ~11 queries per node (load_meta, dir
            // counts, shares, comments, system tags, custom properties).
            // `get_props` runs after `read_dir` for every child, so the batch
            // is always consumed; nodes outside it (the depth-0 root, which
            // dav-server-rs visits before `read_dir`) fall back to the
            // single-row queries.
            let child_ids: Vec<i64> = children.iter().map(|c| c.fileid).collect();
            let (metas, entries): (
                Vec<(String, NcMetaData)>,
                Vec<Result<Box<dyn DavDirEntry>, FsError>>,
            ) = {
                let cache = self.state.mime_cache.read().expect("mime cache lock");
                let mut metas = Vec::with_capacity(children.len());
                let mut entries = Vec::with_capacity(children.len());
                for child in &children {
                    let mime = cache
                        .get_name(child.mimetype)
                        .unwrap_or("application/octet-stream")
                        .to_string();
                    let mut meta = NcMetaData::from_row(child, mime, None);
                    // Apply extended times from the batch map.
                    if let Some(ext) = extended_map.get(&child.fileid) {
                        meta.apply_extended(
                            ext.creation_time,
                            ext.upload_time,
                            ext.metadata_etag.clone(),
                        );
                    }
                    // fc path key — exactly what `load_meta`/`get_props`
                    // look up (both normalize away trailing slashes).
                    let key = child
                        .name
                        .as_ref()
                        .map(|n| format!("{fc_path}/{n}"))
                        .unwrap_or_default();
                    metas.push((key, meta.clone()));
                    entries.push(Ok(Box::new(NcDirEntry { meta }) as Box<dyn DavDirEntry>));
                }
                (metas, entries)
            };

            let batch = &self.propfind_batch;
            {
                let mut meta_cache = batch.meta.lock().expect("propfind batch lock");
                let mut children = batch.children.lock().expect("propfind batch lock");
                let mut child_paths = batch.child_paths.lock().expect("propfind batch lock");
                for (key, meta) in &metas {
                    meta_cache.insert(key.clone(), meta.clone());
                    child_paths.insert(key.clone());
                    children.insert(meta.fileid);
                }
            }
            if !child_ids.is_empty() {
                let child_paths: Vec<String> = metas.iter().map(|(k, _)| k.clone()).collect();
                // One GROUP BY count query for every dir child instead of one
                // per directory ({nc:}contained-folder-count/-file-count).
                let dir_mime_id = nc_db::mime::get_or_insert_mime_id(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &self.state.mime_cache,
                    "httpd/unix-directory",
                )
                .await;
                let counts = row::count_children_batch(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &child_ids,
                    self.storage_id,
                    dir_mime_id,
                )
                .await;
                batch
                    .dir_counts
                    .lock()
                    .expect("propfind batch lock")
                    .extend(counts);
                // Shares, comments, system tags, custom properties — one
                // query per family instead of one per child.
                let details = row::share_details_batch(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &self.uid,
                    &child_ids,
                )
                .await;
                batch
                    .share_details
                    .lock()
                    .expect("propfind batch lock")
                    .extend(details);
                let notes = row::share_notes_batch(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &child_ids,
                )
                .await;
                batch
                    .share_notes
                    .lock()
                    .expect("propfind batch lock")
                    .extend(notes);
                let ccounts = row::comments_counts_batch(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &child_ids,
                )
                .await;
                let unreads = row::comments_unread_batch(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &child_ids,
                    &self.uid,
                )
                .await;
                {
                    let mut comments = batch.comments.lock().expect("propfind batch lock");
                    for id in &child_ids {
                        comments.insert(*id, (*ccounts.get(id).unwrap_or(&0), *unreads.get(id).unwrap_or(&0)));
                    }
                }
                let tags = row::system_tags_batch(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &child_ids,
                )
                .await;
                batch
                    .system_tags
                    .lock()
                    .expect("propfind batch lock")
                    .extend(tags);
                let props = row::custom_properties_batch(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &self.uid,
                    &child_paths,
                )
                .await;
                batch
                    .custom_props
                    .lock()
                    .expect("propfind batch lock")
                    .extend(props);
            }
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
                let old_etag = existing.as_ref().and_then(|r| r.etag.clone());
                let old_storage_mtime = existing.as_ref().map(|r| r.storage_mtime).unwrap_or(0);
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
                    old_etag,
                    old_storage_mtime,
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

            // Update the node itself.  Path/path_hash/name/parent only — PHP
            // `Cache::move` (Cache.php:813-831) leaves the moved file's
            // mtime/etag/storage_mtime untouched (live-verified); the
            // parents' etags are bumped by the propagation below.
            let sql_node = format!(
                "UPDATE {prefix}filecache \
                 SET path=$1, path_hash=$2, name=$3, parent=$4 \
                 WHERE fileid=$5"
            );
            sqlx::query(&sql_node)
                .bind(&to_fc)
                .bind(&new_hash)
                .bind(&new_name)
                .bind(to_parent.fileid)
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
            // Recalculate ancestor sizes (size change can't be expressed as a
            // simple signed delta when subtrees move).  Size-ONLY — PHP's
            // `correctFolderSize` never touches etag/mtime, and the standalone
            // `correct_folder_size`'s internal propagation would re-stamp the
            // root after the target-chain etag, breaking `root == files`.
            if from_parent_fc != to_parent_fc {
                if let Err(e) = self.propagator.correct_folder_size_chain(&from_parent_fc).await {
                    tracing::warn!(path = %from_parent_fc, error = %e, "move: correct_folder_size_chain from failed");
                }
                if let Err(e) = self.propagator.correct_folder_size_chain(&to_parent_fc).await {
                    tracing::warn!(path = %to_parent_fc, error = %e, "move: correct_folder_size_chain to failed");
                }
            } else {
                // Same parent (rename within the same directory) — recalculate once.
                if let Err(e) = self.propagator.correct_folder_size_chain(&to_parent_fc).await {
                    tracing::warn!(path = %to_parent_fc, error = %e, "move: correct_folder_size_chain failed");
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
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    // The copied row is a CLONE of the source (PHP
                    // `copyFromCache`): it inherits the source's etag and
                    // mtime, drops the checksum (NULL), and takes
                    // `storage_mtime` = the copy time (the copied file's disk
                    // mtime — PHP `updateStorageMTimeOnly`).
                    let etag = from_row.etag.clone().unwrap_or_else(|| {
                        format!("{:032x}", uuid::Uuid::new_v4().as_u128())
                    });
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

                    // `checksum` intentionally unbound: the clone drops it
                    // (NULL), matching the oracle.
                    let sql = format!(
                        "INSERT INTO {prefix}filecache \
                         (storage, path, path_hash, parent, name, mimetype, mimepart, \
                          size, mtime, storage_mtime, etag, permissions) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
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
                        .bind(from_row.mtime)
                        .bind(now)
                        .bind(&etag)
                        .bind(from_row.permissions)
                        .fetch_one(&self.state.pool)
                        .await
                    {
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
                        if let Err(e) = sqlx::query(&sql_ext)
                            .bind(fid)
                            .bind(src_creation)
                            .bind(src_upload)
                            .execute(&self.state.pool)
                            .await
                        {
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
            let mut meta = match self.load_meta(&fc_path).await {
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

            // Count direct children for directories (REQ §6.5 contained-*-count).
            // Phase 18.1: read_dir pre-computed every child's counts with one
            // GROUP BY; an in-batch miss means an empty directory (0, 0), and
            // only nodes outside the batch (depth-0 root) run the single query.
            let (child_dirs, child_files) = if meta.is_dir_flag && do_content {
                if batch_contains(&self.propfind_batch.children, &meta.fileid) {
                    batch_get(&self.propfind_batch.dir_counts, &meta.fileid).unwrap_or((0, 0))
                } else {
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
                }
            } else {
                (0, 0)
            };

            // Resolve {oc:}owner-display-name: oc_users.displayname, then oc_accounts, then UID (REQ §6.5 / §4.8).
            // Falls back to the raw UID when no display name is set.
            // Phase 18.1: the value depends only on `uid`, so resolve once
            // per request instead of once per node.
            let owner_display_name = {
                let cached = self
                    .propfind_batch
                    .display_name
                    .lock()
                    .expect("propfind batch lock")
                    .clone();
                match cached {
                    Some(d) => d,
                    None => {
                        let d = row::lookup_user_display_name(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &self.uid,
                        )
                        .await;
                        *self
                            .propfind_batch
                            .display_name
                            .lock()
                            .expect("propfind batch lock") = Some(d.clone());
                        d
                    }
                }
            };

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

            // is_shared: false for home-storage nodes — the file is the user's own.
            // Shared nodes (from oc_share) are detected via is_mounted/share_permissions.
            // Declared early so share_permissions computation can branch on it.
            let is_shared = false;

            // ── Determine if this is the home storage mount root.
            // Used for the displayname fallback, the is-mount-root prop, and
            // compute_share_permissions (mount roots gain DELETE|UPDATE).
            let is_mount_root = matches!(meta.path.as_deref(), Some("") | Some("files"));

            // ── Phase 12.3: sharing mask — match PHP's SetupManager sharing_mask
            // storage wrapper.  When sharing is disabled via shareapi config, the
            // SHARE bit is stripped from ALL cache reads; when sharing is enabled
            // (the normal case) this is a passthrough.
            // Phase 18.1: the sharing mask is uid-only — resolve once per
            // request instead of once per node.
            let sharing_disabled = {
                let cached = self
                    .propfind_batch
                    .sharing_disabled
                    .lock()
                    .expect("propfind batch lock")
                    .clone();
                match cached {
                    Some(s) => s,
                    None => {
                        let s = row::sharing_disabled_for_user(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &self.uid,
                        )
                        .await;
                        *self
                            .propfind_batch
                            .sharing_disabled
                            .lock()
                            .expect("propfind batch lock") = Some(s);
                        s
                    }
                }
            };
            let effective_permissions = row::apply_sharing_mask(meta.permissions, sharing_disabled);

            // NOTE (correction, 2026-07-31): an earlier revision unconditionally
            // stripped PERMISSION_SHARE (16) from the home root here, on the theory
            // that PHP's LazyUserFolder forbids sharing the user root.  That matched
            // only a cold/first-request artifact (a stale capture that seeded
            // SPECS/04-tasks/comparison.md).  Verified against live PHP — both this
            // dev instance (via the proxy's php.dev.local entry) and the reference
            // deployment — the home root reports PERMISSION_SHARE in steady state:
            // `oc:permissions` = RGDNVCK, `ocs:share-permissions` = 31,
            // `ocm:share-permissions` = ["share","read","write"].  The unconditional
            // `& !16` is therefore removed; only the genuine sharing-disabled mask
            // above applies.
            //
            // How PHP can still produce GDNVCK / 15 (observed reproducibly, but
            // transient): when `Root::getUserFolder()` runs before the user's
            // filesystem is set up (`isSetupComplete` false — cold OPCache, right
            // after php-fpm restart, or first touch of the user folder), it returns
            // an *unresolved* `LazyUserFolder`, whose constructor caches
            // `permissions = PERMISSION_ALL ^ PERMISSION_SHARE = 15`
            // (lib/private/Files/Node/LazyUserFolder.php: "Sharing user root folder
            // is not allowed").  `LazyFolder::getPermissions()` returns that cached
            // 15 *only until the folder is resolved*; the first access runs the
            // resolution closure, after which the real home-root permissions (31 →
            // RGDNVCK) are reported.  It is therefore a cold-start window, not the
            // steady state.  We deliberately target the steady state: Rust reads the
            // resolved `oc_filecache` row directly, so it cannot observe that window
            // and does not replicate the transient 15.

            // Update meta so build_props() uses the masked permissions for {oc:}permissions.
            meta.permissions = effective_permissions;

            // ── Phase 12.4: share_permissions — match PHP Node::getSharePermissions().
            // For non-shared nodes (home storage) use the node's own (masked) permissions,
            // with DELETE|UPDATE OR-ed for the mount root, and CREATE|DELETE
            // cleared for files.  For shared nodes (future) use the share's mask.
            let share_permissions = if is_shared {
                // Shared node: use the share's permissions from oc_share.
                row::get_share_max_permissions(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &self.uid,
                    meta.fileid,
                )
                .await
            } else {
                // Own file: derive from the node's (masked) permissions.
                row::compute_share_permissions(effective_permissions, meta.is_dir_flag, is_mount_root)
            };

            // ── Phase 12.4: OCM share-permissions JSON ─────────────────────
            let ocm_share_permissions =
                row::permissions_to_ocm_json(share_permissions);

            // `note`: most-recent non-empty share note for this file.
            // Phase 18.1: batched by read_dir; an in-batch miss means no
            // note, and only nodes outside the batch run the single query.
            let note = if batch_contains(&self.propfind_batch.children, &meta.fileid) {
                batch_get(&self.propfind_batch.share_notes, &meta.fileid).unwrap_or_default()
            } else {
                row::get_share_note(&self.state.pool, &self.state.table_prefix, meta.fileid).await
            };

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

            // ── PHASE-12.5: share-types / sharees ──────────────────────────
            // Phase 18.1: batched by read_dir; single query for nodes
            // outside the batch.
            let share_details = if batch_contains(&self.propfind_batch.children, &meta.fileid) {
                batch_get(&self.propfind_batch.share_details, &meta.fileid).unwrap_or_default()
            } else {
                row::get_share_details(
                    &self.state.pool,
                    &self.state.table_prefix,
                    &self.uid,
                    meta.fileid,
                )
                .await
            };
            let mut share_types: Vec<i32> = share_details
                .iter()
                .map(|d| d.share_type as i32)
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            share_types.sort_unstable();
            let share_types_xml = row::format_share_types_xml(&share_types);
            let sharees_xml = row::format_sharees_xml(&share_details);

            // ── PHASE-12.6: comments properties ────────────────────────────
            // Phase 18.1: read_dir batches both counts per child; single
            // queries for nodes outside the batch.
            let (comments_count, comments_unread, comments_href) = if do_content {
                // Phase 18.1: read_dir batches both counts per child; an
                // in-batch miss means zero comments, and only nodes outside
                // the batch run the single queries.
                let (count, unread) = if batch_contains(&self.propfind_batch.children, &meta.fileid)
                {
                    batch_get(&self.propfind_batch.comments, &meta.fileid).unwrap_or((0, 0))
                } else {
                    let count = row::get_comments_count(
                        &self.state.pool,
                        &self.state.table_prefix,
                        meta.fileid,
                    )
                    .await;
                    let unread = row::get_comments_unread(
                        &self.state.pool,
                        &self.state.table_prefix,
                        meta.fileid,
                        &self.uid,
                    )
                    .await;
                    (count, unread)
                };
                let href = if !self.state.base_url.is_empty() {
                    row::build_comments_href(&self.state.base_url, meta.fileid)
                } else {
                    String::new()
                };
                (count, unread, href)
            } else {
                (0, 0, String::new())
            };

            // ── PHASE-12.7: system tags ────────────────────────────────────
            let system_tags_xml = if do_content {
                let tags = if batch_contains(&self.propfind_batch.children, &meta.fileid) {
                    batch_get(&self.propfind_batch.system_tags, &meta.fileid).unwrap_or_default()
                } else {
                    row::get_system_tags_for_file(
                        &self.state.pool,
                        &self.state.table_prefix,
                        meta.fileid,
                    )
                    .await
                };
                row::format_system_tags_xml(&tags, true)
            } else {
                String::new()
            };

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

            // ── Append Phase 12 extended properties ──────────────────────────
            if do_content {
                crate::props::add_phase12_props(
                    &mut props,
                    &crate::props::Phase12PropCtx {
                        ocm_share_permissions: &ocm_share_permissions,
                        share_types_xml: &share_types_xml,
                        sharees_xml: &sharees_xml,
                        comments_count,
                        comments_unread,
                        comments_href: &comments_href,
                        system_tags_xml: &system_tags_xml,
                    },
                );
            }

            // ── Append custom properties from oc_properties (task §10.11) ─────
            // Phase 18.1: batched by read_dir (keyed by fc path); an in-batch
            // miss means no properties, and only nodes outside the batch run
            // the single query.
            if do_content {
                let custom_props = if batch_contains(&self.propfind_batch.child_paths, fc_path.as_str())
                {
                    batch_get(&self.propfind_batch.custom_props, fc_path.as_str()).unwrap_or_default()
                } else {
                    crate::row::list_custom_properties(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.uid,
                        &fc_path,
                    )
                    .await
                };
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
                            // The dav-server passes the FULL serialized element
                            // in prop.xml (handle_props.rs element_to_davprop_full:
                            // `<oc:favorite xmlns:oc="…">1</oc:favorite>`), so the
                            // inner TEXT must be extracted before the truthy test —
                            // a naive parse of the whole element failed and
                            // executed an un-favorite (finding #15, the "broken
                            // text-valued PROPPATCH extraction").
                            let state = prop
                                .xml
                                .as_deref()
                                .map(|xml| crate::tags::prop_inner_text(xml));
                            let is_fav = state.as_deref().map_or(false, |s| {
                                let t = s.trim();
                                t.parse::<i64>().ok() == Some(1) || t == "true"
                            });
                            // PHP lazily registers the files_metadata appconfig
                            // on the tag/favorite PROPPATCH.
                            ensure_files_metadata_appconfig(&self.state.pool, &self.state.table_prefix)
                                .await;
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
                            // PHP lazily registers the files_metadata appconfig
                            // on the tag/favorite PROPPATCH.
                            ensure_files_metadata_appconfig(&self.state.pool, &self.state.table_prefix)
                                .await;
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

/// Lazily materialize a top-level filecache dir row (PHP does this for
/// `cache/` on the first files access and `uploads/` on the first MKCOL —
/// findings #8/#24; the triggering reads are not identified in the PHP
/// source).  The row is a scanner-insert: size 0, permissions 31, no extended
/// row.  Create-if-missing so subsequent accesses are no-ops, matching PHP's
/// one-shot behavior.
pub(crate) async fn ensure_lazy_dir_row(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    mime_cache: &SharedMimeCache,
    dir_name: &str,
    now: i64,
) {
    if row::lookup_by_path(pool, prefix, storage_id, dir_name)
        .await
        .is_some()
    {
        return;
    }
    let dir_mime_id =
        nc_db::mime::get_or_insert_mime_id(pool, prefix, mime_cache, "httpd/unix-directory").await;
    let dir_mimepart_id = nc_db::mime::get_or_insert_mime_id(pool, prefix, mime_cache, "httpd").await;
    let parent_id = row::lookup_by_path(pool, prefix, storage_id, "")
        .await
        .map(|r| r.fileid)
        .unwrap_or(-1);
    let dir_hash = row::path_hash(dir_name);
    let dir_etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
    let sql = format!(
        "INSERT INTO {prefix}filecache \
         (storage, path, path_hash, parent, name, mimetype, mimepart, \
          size, mtime, storage_mtime, etag, permissions, checksum) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
         ON CONFLICT DO NOTHING",
        prefix = prefix
    );
    if let Err(e) = sqlx::query(&sql)
        .bind(storage_id)
        .bind(dir_name)
        .bind(&dir_hash)
        .bind(parent_id)
        .bind(dir_name)
        .bind(dir_mime_id)
        .bind(dir_mimepart_id)
        .bind(0i64)
        .bind(now)
        .bind(now)
        .bind(&dir_etag)
        .bind(31i32)
        .bind("")
        .execute(pool)
        .await
    {
        warn!(dir = dir_name, error = %e, "lazy dir row materialization failed");
    }
}

/// Lazily register the `core | files_metadata` appconfig row (the lazy
/// registration PHP's PROPPATCH triggers — live-verified against the oracle:
/// `files-live-photo` (etag = "") with `type = 64, lazy = 1`; the `blurhash`
/// key only appears on a fresh instance's first registration and is not
/// reproducible per-run, so it is omitted).
pub(crate) async fn ensure_files_metadata_appconfig(pool: &DbPool, prefix: &str) {
    let config_value = "{\"files-live-photo\":{\"value\":null,\"type\":\"string\",\
        \"etag\":\"\",\"indexed\":false,\"editPermission\":2}}"
        .to_string();
    let sql = format!(
        "INSERT INTO {prefix}appconfig (appid, configkey, configvalue, type, lazy) \
         VALUES ('core', 'files_metadata', $1, 64, 1) \
         ON CONFLICT DO NOTHING",
        prefix = prefix
    );
    if let Err(e) = sqlx::query(&sql).bind(&config_value).execute(pool).await {
        tracing::warn!(error = %e, "files_metadata appconfig registration failed");
    }
}

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

    // ── Delete-to-trash parity (phase-16 findings #6–#12) ────────────────
    //
    // In-memory-SQLite + temp data dir; exercises move_to_trash / delete_file /
    // trash_directory / ensure_parent_dir against the PHP reference behaviour.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, RwLock};

    use nc_db::appconfig::AppConfigCache;
    use nc_db::config::NcConfig;
    use nc_db::filename_validator::FilenameValidator;
    use nc_db::mime::MimeCache;
    use nc_db::pool::DbPool;

    use crate::preview::ProviderRegistry;
    use crate::upload::UploadStateStore;
    use crate::SharedWriteResult;
    use crate::WriteResult;

    use super::{row, NcFileSystem};
    use sqlx::Row as _;

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fresh_data_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nc-dav-trash-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; 26]).unwrap();
    }

    /// The `.d{timestamp}` of the most recent trash operation (from the
    /// `oc_files_trash` row — the naming is derived from the deletion second).
    async fn trash_ts(pool: &DbPool) -> String {
        sqlx::query_scalar::<_, String>("SELECT \"timestamp\" FROM oc_files_trash LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// In-memory SQLite with the delete-path tables and a seeded home tree.
    ///
    /// Seed (fileids fixed, matching the propagator test convention):
    /// ```text
    /// 1  "" (root, size -1)        2  "files" (size 100)
    /// 6  "files_versions" (0)      4  "files/hello.txt" (26)
    /// ```
    async fn fresh_delete_db() -> (DbPool, String, i64) {
        sqlx::any::install_default_drivers();
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite");

        sqlx::query(
            "CREATE TABLE oc_filecache (
                fileid           INTEGER NOT NULL PRIMARY KEY,
                storage          BIGINT  NOT NULL DEFAULT 0,
                path             VARCHAR(4000),
                path_hash        VARCHAR(32) NOT NULL DEFAULT '',
                parent           BIGINT  NOT NULL DEFAULT 0,
                name             VARCHAR(250),
                mimetype         BIGINT  NOT NULL DEFAULT 0,
                mimepart         BIGINT  NOT NULL DEFAULT 0,
                size             BIGINT  NOT NULL DEFAULT 0,
                mtime            INTEGER NOT NULL DEFAULT 0,
                storage_mtime    INTEGER NOT NULL DEFAULT 0,
                etag             VARCHAR(40),
                permissions      INTEGER DEFAULT 0,
                checksum         VARCHAR(255)
            )",
        )
        .execute(&pool)
        .await
        .expect("create filecache");
        sqlx::query(
            "CREATE TABLE oc_filecache_extended (
                fileid         INTEGER NOT NULL PRIMARY KEY,
                metadata_etag  VARCHAR(40) NOT NULL DEFAULT '',
                creation_time  BIGINT NOT NULL DEFAULT 0,
                upload_time    BIGINT NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("create filecache_extended");
        sqlx::query(
            "CREATE TABLE oc_files_versions (
                id         INTEGER NOT NULL PRIMARY KEY,
                file_id    BIGINT NOT NULL,
                \"timestamp\" BIGINT NOT NULL,
                size       BIGINT NOT NULL,
                mimetype   BIGINT NOT NULL,
                metadata   TEXT
            )",
        )
        .execute(&pool)
        .await
        .expect("create files_versions");
        sqlx::query(
            "CREATE TABLE oc_files_trash (
                id         VARCHAR(250) NOT NULL,
                \"user\"      VARCHAR(64) NOT NULL,
                \"timestamp\" VARCHAR(12) NOT NULL,
                location   VARCHAR(512) NOT NULL,
                deleted_by VARCHAR(64) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("create files_trash");
        sqlx::query(
            "CREATE TABLE oc_preview_generation (
                id        INTEGER NOT NULL PRIMARY KEY,
                uid       VARCHAR(64) NOT NULL,
                file_id   BIGINT NOT NULL,
                queued_at BIGINT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("create preview_generation");
        sqlx::query(
            "CREATE TABLE oc_mimetypes (
                id       BIGINT NOT NULL PRIMARY KEY,
                mimetype VARCHAR(255) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("create mimetypes");
        sqlx::query(
            "CREATE TABLE oc_appconfig (
                appid      VARCHAR(32) NOT NULL,
                configkey  VARCHAR(64) NOT NULL,
                configvalue VARCHAR(4000) NOT NULL,
                type       INTEGER NOT NULL DEFAULT 0,
                lazy       INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("create appconfig");

        // files_trashbin enabled.
        sqlx::query(
            "INSERT INTO oc_appconfig (appid, configkey, configvalue, type, lazy) \
             VALUES ('files_trashbin', 'enabled', 'yes', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed appconfig");

        let prefix = "oc_".to_string();
        let storage_id = 1i64;
        for (fid, path, parent, size, name) in [
            (1i64, "", -1i64, -1i64, ""),
            (2, "files", 1, 100, "files"),
            (6, "files_versions", 1, 0, "files_versions"),
            (4, "files/hello.txt", 2, 26, "hello.txt"),
        ] {
            sqlx::query(&format!(
                "INSERT INTO {prefix}filecache \
                 (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
                  size, mtime, storage_mtime, etag, permissions, checksum) \
                 VALUES ($1, $2, $3, $4, $5, $6, 0, 0, $7, 100, 100, 'etag', 27, '')"
            ))
            .bind(fid)
            .bind(storage_id)
            .bind(path)
            .bind(row::path_hash(path))
            .bind(parent)
            .bind(name)
            .bind(size)
            .execute(&pool)
            .await
            .expect("seed filecache");
        }

        (pool, prefix, storage_id)
    }

    fn test_fs(
        pool: DbPool,
        prefix: String,
        storage_id: i64,
        data_dir: PathBuf,
    ) -> NcFileSystem {
        let cfg = NcConfig::from_php_config("<?php\n$CONFIG = ['dbtype' => 'sqlite3'];").unwrap();
        let state = crate::NcDavState {
            pool,
            mime_cache: Arc::new(RwLock::new(MimeCache::default())),
            appconfig_cache: Arc::new(RwLock::new(AppConfigCache::default())),
            table_prefix: prefix,
            data_directory: data_dir,
            instance_id: Arc::new("testinst".to_string()),
            filename_validator: Arc::new(FilenameValidator::from_config(&cfg)),
            base_url: Arc::new(String::new()),
            upload_state_store: Arc::new(UploadStateStore::new()),
            preview_registry: Arc::new(ProviderRegistry::build(false, None, false, false, false, &[])),
        };
        let write_result: SharedWriteResult = Arc::new(std::sync::Mutex::new(None::<WriteResult>));
        let put_error: crate::SharedPutError = Arc::new(std::sync::Mutex::new(None));
        NcFileSystem::new(
            state,
            "admin".to_string(),
            storage_id,
            None,
            None,
            write_result,
            put_error,
            false,
        )
    }

    async fn fc_row(
        pool: &DbPool,
        prefix: &str,
        path: &str,
    ) -> Option<(i64, i64, i64, i64, i64, String)> {
        // (fileid, size, mtime, storage_mtime, parent, path)
        let hash = row::path_hash(path);
        let sql = format!(
            "SELECT fileid, size, mtime, storage_mtime, parent, path FROM {prefix}filecache \
             WHERE storage = $1 AND path_hash = $2",
            prefix = prefix
        );
        sqlx::query(&sql)
            .bind(1i64)
            .bind(&hash)
            .fetch_optional(pool)
            .await
            .ok()?
            .map(|r| {
                (
                    r.get::<i64, _>("fileid"),
                    r.get::<i64, _>("size"),
                    r.get::<i64, _>("mtime"),
                    r.get::<i64, _>("storage_mtime"),
                    r.get::<i64, _>("parent"),
                    r.get::<String, _>("path"),
                )
            })
    }

    async fn extended_count(pool: &DbPool) -> i64 {
        let sql = "SELECT COUNT(*) FROM oc_filecache_extended";
        sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await.unwrap()
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
                row::lookup_by_path(&pool, &prefix, storage_id, p).await.is_some(),
                "missing skeleton dir {p}"
            );
        }

        // oc_files_trash row inserted.
        let trash_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oc_files_trash")
            .fetch_one(&pool)
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
        let (_, size, _, _, _, _) = fc_row(&pool, &prefix, "files_trashbin/files").await.unwrap();
        assert_eq!(size, 26, "files_trashbin/files must gain the trashed file's size");
        let (_, size, _, _, _, _) = fc_row(&pool, &prefix, "files_trashbin").await.unwrap();
        assert_eq!(size, 26, "files_trashbin must gain the trashed file's size");

        // Root recomputed from children: files(74) + files_trashbin(26) + files_versions(0).
        let (_, size, _, _, _, _) = fc_row(&pool, &prefix, "").await.unwrap();
        assert_eq!(size, 100);
    }

    /// Finding #9: ensure_parent_dir creates filecache dirs WITHOUT
    /// oc_filecache_extended rows (PHP `View::mkdir` → `Cache::insert` writes
    /// no extension fields for directories).
    #[tokio::test]
    async fn ensure_parent_dir_creates_no_extended_rows() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir);

        fs.ensure_parent_dir("files_trashbin/files/x").await.unwrap();
        for p in ["files_trashbin", "files_trashbin/files", "files_trashbin/files/x"] {
            assert!(
                row::lookup_by_path(&pool, &prefix, storage_id, p).await.is_some(),
                "missing dir {p}"
            );
        }
        assert_eq!(extended_count(&pool).await, 0, "no extended rows for mkdir'd dirs");
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
        sqlx::query(
            "INSERT INTO oc_filecache \
             (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
              size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES (5, 1, 'files_versions/hello.txt.v100', $1, 6, 'hello.txt.v100', 0, 0, 26, 100, 100, 'etag', 27, '')",
        )
        .bind(row::path_hash("files_versions/hello.txt.v100"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO oc_files_versions (id, file_id, \"timestamp\", size, mimetype, metadata) \
             VALUES (1, 4, 100, 26, 0, '{\"author\":\"admin\"}')",
        )
        .execute(&pool)
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
            !data_dir.join("admin/files_versions/hello.txt.v100").exists(),
            "version file must leave files_versions/"
        );

        // oc_files_versions row deleted (PHP remove_hook → deleteVersionsEntity).
        let vcount: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oc_files_versions")
            .fetch_one(&pool)
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
            sqlx::query(&format!(
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
            .execute(&pool)
            .await
            .unwrap();
        }
        // The seeded `files/hello.txt` row references parent 2 — leave it; the
        // subtree DELETE only touches files/dir rows.  Drop the hello.txt row
        // so it doesn't linger under the (now-trashed) files/ tree.
        sqlx::query("DELETE FROM oc_filecache WHERE fileid = 4")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO oc_files_versions (id, file_id, \"timestamp\", size, mimetype, metadata) \
             VALUES (1, 9, 100, 8, 0, '{\"author\":\"admin\"}')",
        )
        .execute(&pool)
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
        let vcount: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oc_files_versions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(vcount, 1, "dir trash must not delete inner files' version rows");

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
        sqlx::query(
            "INSERT INTO oc_preview_generation (id, uid, file_id, queued_at) \
             VALUES (1, 'admin', 4, 100)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir);
        fs.delete_file("files/hello.txt").await.unwrap();

        let pcount: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oc_preview_generation")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(pcount, 1, "preview row must survive the trash move");
    }

    async fn etag_of(pool: &DbPool, prefix: &str, path: &str) -> Option<String> {
        let hash = row::path_hash(path);
        let sql = format!(
            "SELECT etag FROM {prefix}filecache WHERE storage = $1 AND path_hash = $2",
            prefix = prefix
        );
        sqlx::query_scalar::<_, String>(&sql)
            .bind(1i64)
            .bind(&hash)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    /// Finding #10: the `oc_files_versions` cleanup is unconditional — the row
    /// the SUT's PUT inserted is deleted even when no version FILE exists on
    /// disk (live-verified PHP behavior).
    #[tokio::test]
    async fn delete_file_deletes_version_row_without_version_files() {
        let (pool, prefix, storage_id) = fresh_delete_db().await;
        let data_dir = fresh_data_dir();
        touch(&data_dir.join("admin/files/hello.txt"));
        sqlx::query(
            "INSERT INTO oc_files_versions (id, file_id, \"timestamp\", size, mimetype, metadata) \
             VALUES (1, 4, 100, 26, 0, '{\"author\":\"admin\"}')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let fs = test_fs(pool.clone(), prefix.clone(), storage_id, data_dir);
        fs.delete_file("files/hello.txt").await.unwrap();

        let vcount: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oc_files_versions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(vcount, 0, "version rows must be deleted even without version files");
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
        let trash_files = etag_of(&pool, &prefix, "files_trashbin/files").await.unwrap();
        let keys = etag_of(&pool, &prefix, "files_trashbin/keys").await.unwrap();
        let versions = etag_of(&pool, &prefix, "files_trashbin/versions").await.unwrap();
        assert_eq!(root, files, "root and files/ must share the source-chain etag");
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
        assert_eq!(extended_count(&pool).await, 0, "cache row must have no extended row");
    }
}
