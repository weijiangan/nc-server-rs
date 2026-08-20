//! File versions on overwrite (REQ §6.9 / Phase 9.4).
//!
//! When a file is overwritten via PUT, the prior content is saved to
//! `files_versions/{relativePath}.v{old_mtime}` so the PHP-FPM versions
//! endpoint can browse and restore it.
//!
//! PHP reference:
//! - `apps/files_versions/lib/Storage.php` (store, renameOrCopy)
//! - `apps/files_versions/lib/Versions/LegacyVersionsBackend.php` (createVersion)
//! - `apps/files_versions/lib/Listener/FileEventsListener.php` (rename/copy hooks)

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use nc_db::mime::SharedMimeCache;
use nc_db::pool::DbPool;
use tracing::{debug, warn};

use crate::row;

/// Save the pre-overwrite file content as a version.
///
/// Called from `davfile::flush()` **before** the temp→final rename, while the
/// old file still exists on disk at `final_disk_path`.
///
/// Returns `Ok(())` if a version was created, or `Ok(())` if the operation
/// was skipped (empty file, etc.).  Errors are logged but do not fail the PUT.
pub async fn store_version(
    pool: &DbPool,
    prefix: &str,
    data_dir: &Path,
    uid: &str,
    storage_id: i64,
    mime_cache: &SharedMimeCache,
    fc_path: &str,
    final_disk_path: &Path,
    old_size: i64,
    old_mtime: i64,
    old_mimetype: i64,
    old_permissions: i32,
    old_creation_time: i64,
    old_upload_time: i64,
    source_fileid: i64,
    old_etag: &str,
    dir_mime_id: i64,
    dir_mimepart_id: i64,
) {
    debug!(
        fc_path,
        old_size, old_mtime, old_mimetype, source_fileid, "store_version called"
    );

    // Guard: skip .part files, empty files, directories (PHP Storage::store:160-205).
    if fc_path.ends_with(".part") {
        return;
    }
    if old_size == 0 {
        return;
    }
    // Check if the old file is a directory.
    {
        let cache = mime_cache.read().expect("mime cache lock");
        if cache.get_name(old_mimetype).as_deref() == Some("httpd/unix-directory") {
            return;
        }
        // Guard dropped here — safe since we return early or continue without holding it.
    }

    let relative = match fc_path.strip_prefix("files/") {
        Some(r) => r,
        None => {
            warn!("store_version: path not under files/: {fc_path}");
            return;
        }
    };

    // Build version paths.
    let version_fc = format!("files_versions/{relative}.v{old_mtime}");
    let version_disk = crate::row::disk_path(data_dir, uid, &version_fc);

    // Create parent directories on disk.
    if let Some(parent) = version_disk.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            warn!("store_version: failed to create parent dirs for {version_fc}: {e}");
            return;
        }
    }

    // Copy old file content to the version path on disk.
    if let Err(e) = tokio::fs::copy(final_disk_path, &version_disk).await {
        warn!("store_version: disk copy failed for {version_fc}: {e}");
        return;
    }

    // Auto-create parent directories in filecache under files_versions/.
    if let Err(e) = ensure_version_parents(
        pool,
        prefix,
        storage_id,
        data_dir,
        uid,
        &version_fc,
        dir_mime_id,
        dir_mimepart_id,
    )
    .await
    {
        warn!("store_version: ensure_version_parents failed: {e}");
        return;
    }

    // Insert a filecache row for the version so the PHP-FPM versions
    // PROPFIND can enumerate it (PHP LegacyVersionsBackend::createVersion:160-162).
    // §9.4: inherit permissions, creation_time, and upload_time from the source file;
    // the etag is the SOURCE file's etag (the row is a clone — PHP `View::copy` →
    // `Cache::copyFromCache` copies the source row as-is), and `storage_mtime` is the
    // copied file's disk mtime (the copy time — PHP `updateStorageMTimeOnly`).
    let now = current_timestamp();
    insert_version_row(
        pool,
        prefix,
        storage_id,
        mime_cache,
        &version_fc,
        old_size,
        old_mtime,
        old_mimetype,
        old_permissions,
        old_creation_time,
        old_upload_time,
        old_etag,
        now,
    )
    .await;

    // PHP `View::copy` → `copyOrRenameFromStorage` side effects
    // (Updater.php:192-204) + the parent-size recompute PHP gets from
    // `getFileInfo` ("ensure the file is scanned", LegacyVersionsBackend:165):
    // - the version file's parent dir gets `storage_mtime` = its disk mtime
    // - every ancestor (root, `files_versions`) gets one shared etag + mtime bump
    // - the parent dir's size is recomputed from its children
    let propagator =
        crate::propagator::Propagator::new(pool.clone(), prefix.to_string(), storage_id);
    let parent_fc = version_fc
        .rsplit_once('/')
        .map(|(p, _)| p.to_string())
        .unwrap_or_default();
    let parent_disk = crate::row::disk_path(data_dir, uid, &parent_fc);
    if let Err(e) = propagator
        .correct_parent_storage_mtime(&parent_fc, &parent_disk)
        .await
    {
        warn!(parent = %parent_fc, error = %e, "store_version: parent storage_mtime correction failed");
    }
    if let Err(e) = propagator.propagate_change(&version_fc, now, 0).await {
        warn!(version = %version_fc, error = %e, "store_version: version-chain propagation failed");
    }
    if let Err(e) = propagator.correct_folder_size_chain(&version_fc).await {
        warn!(version = %version_fc, error = %e, "store_version: version-chain size recompute failed");
    }
}

/// Relocate versions when a file is renamed or moved.
///
/// For files: moves all `files_versions/{oldRel}.v*` to `files_versions/{newRel}.v*`.
/// For directories: moves the entire `files_versions/{oldRel}/` subtree.
pub async fn rename_versions(
    pool: &DbPool,
    prefix: &str,
    data_dir: &Path,
    uid: &str,
    storage_id: i64,
    old_fc_path: &str,
    new_fc_path: &str,
) {
    let old_rel = match old_fc_path.strip_prefix("files/") {
        Some(r) => r,
        None => return,
    };
    let new_rel = match new_fc_path.strip_prefix("files/") {
        Some(r) => r,
        None => return,
    };

    let old_versions_dir =
        crate::row::disk_path(data_dir, uid, &format!("files_versions/{old_rel}"));
    let new_versions_dir =
        crate::row::disk_path(data_dir, uid, &format!("files_versions/{new_rel}"));
    let old_versions_fc = format!("files_versions/{old_rel}");
    let new_versions_fc = format!("files_versions/{new_rel}");

    // Check if the old path is a directory or a file (by checking disk).
    if old_versions_dir.is_dir() {
        // Directory: move the whole versions subdirectory.
        if let Some(parent) = new_versions_dir.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                debug!(parent = %parent.display(), error = %e, "Failed to create version parent dir");
            }
        }
        if let Err(e) = tokio::fs::rename(&old_versions_dir, &new_versions_dir).await {
            warn!("rename_versions: directory rename failed {old_versions_dir:?} → {new_versions_dir:?}: {e}");
        }
        // PHP `View::move` → `Cache::move`: repath the whole cache subtree.
        repath_version_subtree(pool, prefix, storage_id, &old_versions_fc, &new_versions_fc).await;
    } else {
        // File: find and rename individual version files matching .v{ts}.
        if let Some(parent) = old_versions_dir.parent() {
            let prefix_pattern = old_versions_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Ok(mut entries) = tokio::fs::read_dir(parent).await {
                let mut renamed: Vec<(String, String)> = Vec::new();
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with(&prefix_pattern)
                        && name_str[prefix_pattern.len()..].starts_with(".v")
                    {
                        let new_name = format!("{}{}", new_rel, &name_str[prefix_pattern.len()..]);
                        let new_disk = crate::row::disk_path(
                            data_dir,
                            uid,
                            &format!("files_versions/{new_name}"),
                        );
                        if let Some(np) = new_disk.parent() {
                            let _ = tokio::fs::create_dir_all(np).await;
                        }
                        if let Err(e) = tokio::fs::rename(&entry.path(), &new_disk).await {
                            warn!("rename_versions: file rename failed {name_str}: {e}");
                        } else {
                            renamed.push((name_str.to_string(), new_name));
                        }
                    }
                }
                for (old_name, new_name) in renamed {
                    repath_version_row(
                        pool,
                        prefix,
                        storage_id,
                        &format!("files_versions/{old_name}"),
                        &format!("files_versions/{new_name}"),
                    )
                    .await;
                }
            }
        }
    }
}

/// Repath the `oc_filecache` rows of a whole `files_versions/{old}/...` subtree
/// to `files_versions/{new}/...`, matching PHP's `Cache::move` for a directory
/// move (path/path_hash updated for every row; the moved node's `name` is also
/// rewritten, while descendants keep theirs).
async fn repath_version_subtree(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    old_fc: &str,
    new_fc: &str,
) {
    if old_fc == new_fc {
        return;
    }
    let like = format!("{old_fc}/%");
    let sql_fetch = format!(
        "SELECT fileid, path FROM {prefix}filecache \
         WHERE storage = $1 AND (path = $2 OR path LIKE $3)"
    );
    let rows: Vec<(i64, String)> = match pool {
        DbPool::Pg(p) => sqlx::query_as::<sqlx::Postgres, (i64, String)>(&sql_fetch)
            .bind(storage_id)
            .bind(old_fc)
            .bind(&like)
            .fetch_all(p)
            .await
            .unwrap_or_default(),
        DbPool::Sqlite(p) => sqlx::query_as::<sqlx::Sqlite, (i64, String)>(&sql_fetch)
            .bind(storage_id)
            .bind(old_fc)
            .bind(&like)
            .fetch_all(p)
            .await
            .unwrap_or_default(),
    };

    // The moved node's new parent path (same parent dir for a rename within the
    // versions tree, but recompute so a nested rename lands under the right dir).
    let moved_parent_fc = {
        let mut parts: Vec<&str> = new_fc.split('/').collect();
        parts.pop();
        parts.join("/")
    };
    let moved_parent_id =
        row::lookup_by_path(pool, prefix, storage_id, &moved_parent_fc).await.map(|r| r.fileid);

    for (fileid, old_path) in rows {
        let new_path = if old_path == old_fc {
            new_fc.to_string()
        } else {
            format!("{new_fc}{}", &old_path[old_fc.len()..])
        };
        let new_hash = row::path_hash(&new_path);
        if old_path == old_fc {
            // The moved node itself: PHP `Cache::move` rewrites path, path_hash,
            // name AND parent.
            let new_name = new_fc.rsplit('/').next().unwrap_or("").to_string();
            match moved_parent_id {
                Some(pid) => {
                    let sql = format!(
                        "UPDATE {prefix}filecache SET path=$1, path_hash=$2, name=$3, parent=$4 WHERE fileid=$5"
                    );
                    let r = match pool {
                        DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&sql)
                            .bind(&new_path).bind(&new_hash).bind(&new_name).bind(pid).bind(fileid)
                            .execute(p).await.map(|_| ()),
                        DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&sql)
                            .bind(&new_path).bind(&new_hash).bind(&new_name).bind(pid).bind(fileid)
                            .execute(p).await.map(|_| ()),
                    };
                    if let Err(e) = r {
                        warn!(fileid, error = %e, "rename_versions: failed to repath cache row {old_path}");
                    }
                }
                None => {
                    warn!(new_path, "rename_versions: moved version dir parent not found; leaving parent unchanged");
                    let sql = format!(
                        "UPDATE {prefix}filecache SET path=$1, path_hash=$2, name=$3 WHERE fileid=$4"
                    );
                    let r = match pool {
                        DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&sql)
                            .bind(&new_path).bind(&new_hash).bind(&new_name).bind(fileid)
                            .execute(p).await.map(|_| ()),
                        DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&sql)
                            .bind(&new_path).bind(&new_hash).bind(&new_name).bind(fileid)
                            .execute(p).await.map(|_| ()),
                    };
                    if let Err(e) = r {
                        warn!(fileid, error = %e, "rename_versions: failed to repath cache row {old_path}");
                    }
                }
            }
        } else {
            // Descendants: path/path_hash only (parents move with the subtree).
            let sql = format!(
                "UPDATE {prefix}filecache SET path=$1, path_hash=$2 WHERE fileid=$3"
            );
            let r = match pool {
                DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&sql)
                    .bind(&new_path).bind(&new_hash).bind(fileid)
                    .execute(p).await.map(|_| ()),
                DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&sql)
                    .bind(&new_path).bind(&new_hash).bind(fileid)
                    .execute(p).await.map(|_| ()),
            };
            if let Err(e) = r {
                warn!(fileid, error = %e, "rename_versions: failed to repath cache row {old_path}");
            }
        }
    }
}

/// Repath a single version FILE's `oc_filecache` row, matching PHP's
/// `Cache::move` for a single file (path/path_hash/name/parent rewritten).
async fn repath_version_row(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    old_fc: &str,
    new_fc: &str,
) {
    if old_fc == new_fc {
        return;
    }
    let new_hash = row::path_hash(new_fc);
    let new_name = new_fc.rsplit('/').next().unwrap_or("").to_string();
    let new_parent_fc = {
        let mut parts: Vec<&str> = new_fc.split('/').collect();
        parts.pop();
        parts.join("/")
    };
    // PHP `Cache::move` recomputes the moved node's parent from the target path.
    match row::lookup_by_path(pool, prefix, storage_id, &new_parent_fc).await {
        Some(parent) => {
            let sql = format!(
                "UPDATE {prefix}filecache SET path=$1, path_hash=$2, name=$3, parent=$4 \
                 WHERE storage=$5 AND path=$6"
            );
            let result = match pool {
                DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&sql)
                    .bind(new_fc).bind(&new_hash).bind(&new_name).bind(parent.fileid)
                    .bind(storage_id).bind(old_fc)
                    .execute(p).await.map(|_| ()),
                DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&sql)
                    .bind(new_fc).bind(&new_hash).bind(&new_name).bind(parent.fileid)
                    .bind(storage_id).bind(old_fc)
                    .execute(p).await.map(|_| ()),
            };
            if let Err(e) = result {
                warn!(old_fc, new_fc, error = %e, "rename_versions: failed to repath version row");
            }
        }
        None => {
            warn!(new_fc, "rename_versions: new parent not found for version row; leaving parent unchanged");
            let sql = format!(
                "UPDATE {prefix}filecache SET path=$1, path_hash=$2, name=$3 \
                 WHERE storage=$4 AND path=$5"
            );
            let result = match pool {
                DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&sql)
                    .bind(new_fc).bind(&new_hash).bind(&new_name)
                    .bind(storage_id).bind(old_fc)
                    .execute(p).await.map(|_| ()),
                DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&sql)
                    .bind(new_fc).bind(&new_hash).bind(&new_name)
                    .bind(storage_id).bind(old_fc)
                    .execute(p).await.map(|_| ()),
            };
            if let Err(e) = result {
                warn!(old_fc, new_fc, error = %e, "rename_versions: failed to repath version row");
            }
        }
    }
}

/// Copy versions when a file is copied.
///
/// For files: copies all `files_versions/{oldRel}.v*` to `files_versions/{newRel}.v*`.
/// For directories: copies the entire `files_versions/{oldRel}/` subtree.
///
/// Mirrors PHP `Storage::renameOrCopy` (copy) → `View::copy` → `Cache::copyFromCache`:
/// besides copying the version FILES on disk, each copied version's `oc_filecache`
/// row is CLONED to the target path (new fileid, cloned fields) so the versions
/// PROPFIND can enumerate the copies.
pub async fn copy_versions(
    pool: &DbPool,
    prefix: &str,
    data_dir: &Path,
    uid: &str,
    storage_id: i64,
    old_fc_path: &str,
    new_fc_path: &str,
) {
    let old_rel = match old_fc_path.strip_prefix("files/") {
        Some(r) => r,
        None => return,
    };
    let new_rel = match new_fc_path.strip_prefix("files/") {
        Some(r) => r,
        None => return,
    };

    let old_versions_dir =
        crate::row::disk_path(data_dir, uid, &format!("files_versions/{old_rel}"));
    let new_versions_dir =
        crate::row::disk_path(data_dir, uid, &format!("files_versions/{new_rel}"));
    let old_versions_fc = format!("files_versions/{old_rel}");
    let new_versions_fc = format!("files_versions/{new_rel}");

    if old_versions_dir.is_dir() {
        // Directory: copy the whole versions subdirectory on disk, then clone
        // the cache subtree (PHP `View::copy` → `Cache::copyFromCache` recursion).
        if let Some(parent) = new_versions_dir.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                debug!(parent = %parent.display(), error = %e, "Failed to create version parent dir");
            }
        }
        copy_version_tree(&old_versions_dir, &new_versions_dir);
        clone_version_subtree(pool, prefix, storage_id, &old_versions_fc, &new_versions_fc).await;
    } else {
        // File: copy each .v{ts} version file and clone its cache row.
        if let Some(parent) = old_versions_dir.parent() {
            let prefix_pattern = old_versions_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Ok(mut entries) = tokio::fs::read_dir(parent).await {
                let mut copied: Vec<(String, String)> = Vec::new();
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with(&prefix_pattern)
                        && name_str[prefix_pattern.len()..].starts_with(".v")
                    {
                        let new_name = format!("{}{}", new_rel, &name_str[prefix_pattern.len()..]);
                        let new_disk = crate::row::disk_path(
                            data_dir,
                            uid,
                            &format!("files_versions/{new_name}"),
                        );
                        if let Some(np) = new_disk.parent() {
                            let _ = tokio::fs::create_dir_all(np).await;
                        }
                        if let Err(e) = tokio::fs::copy(&entry.path(), &new_disk).await {
                            warn!("copy_versions: file copy failed {name_str}: {e}");
                        } else {
                            copied.push((name_str.to_string(), new_name));
                        }
                    }
                }
                for (old_name, new_name) in copied {
                    clone_version_file(
                        pool,
                        prefix,
                        storage_id,
                        &format!("files_versions/{old_name}"),
                        &format!("files_versions/{new_name}"),
                    )
                    .await;
                }
            }
        }
    }
}

/// Recursively copy a version directory tree on disk.
fn copy_version_tree(src: &Path, dst: &Path) {
    if let Err(e) = std::fs::create_dir_all(dst) {
        warn!(src = %src.display(), dst = %dst.display(), error = %e, "copy_versions: create dir failed");
        return;
    }
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_version_tree(&from, &to);
            } else if let Err(e) = std::fs::copy(&from, &to) {
                warn!(src = %from.display(), dst = %to.display(), error = %e, "copy_versions: file copy failed");
            }
        }
    }
}

/// Clone a single version FILE's `oc_filecache` row to a new path (new fileid,
/// cloned fields), matching PHP `Cache::copyFromCache` for a file entry.
async fn clone_version_file(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    old_fc: &str,
    new_fc: &str,
) {
    if old_fc == new_fc {
        return;
    }
    let Some(src) = row::lookup_by_path(pool, prefix, storage_id, old_fc).await else {
        return;
    };
    let parent_fc = {
        let mut parts: Vec<&str> = new_fc.split('/').collect();
        parts.pop();
        parts.join("/")
    };
    let Some(parent) = row::lookup_by_path(pool, prefix, storage_id, &parent_fc).await else {
        warn!(new_fc, "copy_versions: target parent not found");
        return;
    };
    insert_version_clone(pool, prefix, storage_id, new_fc, parent.fileid, &src).await;
}

/// Insert a new `oc_filecache` row cloning `src` (and its `oc_filecache_extended`
/// row) at `new_fc` under `parent_id`.  Returns the new fileid (0 on failure).
async fn insert_version_clone(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    new_fc: &str,
    parent_id: i64,
    src: &row::FileCacheRow,
) -> i64 {
    let hash = row::path_hash(new_fc);
    let name = new_fc.rsplit('/').next().unwrap_or("").to_string();
    let sql = format!(
        "INSERT INTO {prefix}filecache \
         (storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime, \
          storage_mtime, etag, permissions, checksum) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) RETURNING fileid"
    );
    let fetched: Result<i64, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query_scalar::<sqlx::Postgres, _>(&sql)
            .bind(storage_id)
            .bind(new_fc)
            .bind(&hash)
            .bind(parent_id)
            .bind(&name)
            .bind(src.mimetype)
            .bind(src.mimepart)
            .bind(src.size)
            .bind(src.mtime)
            .bind(src.storage_mtime)
            .bind(src.etag.as_deref().unwrap_or(""))
            .bind(src.permissions)
            .bind(src.checksum.as_deref().unwrap_or(""))
            .fetch_one(p)
            .await,
        DbPool::Sqlite(p) => sqlx::query_scalar::<sqlx::Sqlite, _>(&sql)
            .bind(storage_id)
            .bind(new_fc)
            .bind(&hash)
            .bind(parent_id)
            .bind(&name)
            .bind(src.mimetype)
            .bind(src.mimepart)
            .bind(src.size)
            .bind(src.mtime)
            .bind(src.storage_mtime)
            .bind(src.etag.as_deref().unwrap_or(""))
            .bind(src.permissions)
            .bind(src.checksum.as_deref().unwrap_or(""))
            .fetch_one(p)
            .await,
    };
    let new_id = match fetched {
        Ok(id) => id,
        Err(e) => {
            warn!(new_fc, error = %e, "copy_versions: insert clone failed");
            return 0;
        }
    };

    // Clone the extended (creation_time/upload_time/metadata_etag) row too.
    let ext = row::get_extended(pool, prefix, src.fileid).await;
    let sql_ext = format!(
        "INSERT INTO {prefix}filecache_extended (fileid, metadata_etag, creation_time, upload_time) \
         VALUES ($1, $2, $3, $4) ON CONFLICT(fileid) DO NOTHING"
    );
    let _ = match pool {
        DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&sql_ext)
            .bind(new_id)
            .bind(ext.metadata_etag.as_deref().unwrap_or(""))
            .bind(ext.creation_time)
            .bind(ext.upload_time)
            .execute(p)
            .await
            .map(|_| ()),
        DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&sql_ext)
            .bind(new_id)
            .bind(ext.metadata_etag.as_deref().unwrap_or(""))
            .bind(ext.creation_time)
            .bind(ext.upload_time)
            .execute(p)
            .await
            .map(|_| ()),
    };
    new_id
}

/// Clone the whole `files_versions/{old}/...` cache subtree to `files_versions/{new}/...`,
/// matching PHP `Cache::copyFromCache` recursion (parents before children, so the
/// new parent ids are known when each child is cloned).
async fn clone_version_subtree(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    old_fc: &str,
    new_fc: &str,
) {
    if old_fc == new_fc {
        return;
    }
    let like = format!("{old_fc}/%");
    let sql_fetch = format!(
        "SELECT fileid, path, parent FROM {prefix}filecache \
         WHERE storage = $1 AND (path = $2 OR path LIKE $3) \
         ORDER BY length(path) ASC"
    );
    let rows: Vec<(i64, String, i64)> = match pool {
        DbPool::Pg(p) => sqlx::query_as::<sqlx::Postgres, (i64, String, i64)>(&sql_fetch)
            .bind(storage_id)
            .bind(old_fc)
            .bind(&like)
            .fetch_all(p)
            .await
            .unwrap_or_default(),
        DbPool::Sqlite(p) => sqlx::query_as::<sqlx::Sqlite, (i64, String, i64)>(&sql_fetch)
            .bind(storage_id)
            .bind(old_fc)
            .bind(&like)
            .fetch_all(p)
            .await
            .unwrap_or_default(),
    };

    let mut remap: HashMap<i64, i64> = HashMap::new();
    for (old_id, old_path, old_parent) in rows {
        let new_path = if old_path == old_fc {
            new_fc.to_string()
        } else {
            format!("{new_fc}{}", &old_path[old_fc.len()..])
        };
        let parent_id = if let Some(p) = remap.get(&old_parent) {
            *p
        } else {
            let parent_fc = {
                let mut parts: Vec<&str> = new_path.split('/').collect();
                parts.pop();
                parts.join("/")
            };
            match row::lookup_by_path(pool, prefix, storage_id, &parent_fc).await {
                Some(p) => p.fileid,
                None => {
                    warn!(new_path, "copy_versions: subtree target parent not found");
                    continue;
                }
            }
        };
        let Some(src) = row::lookup_by_path(pool, prefix, storage_id, &old_path).await else {
            continue;
        };
        let new_id = insert_version_clone(pool, prefix, storage_id, &new_path, parent_id, &src).await;
        if new_id != 0 {
            remap.insert(old_id, new_id);
        }
    }
}

// ─── Internal helpers ──────────────────────────────────────────────────────────

/// Ensure the parent chain for a version path exists in filecache.
async fn ensure_version_parents(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    data_dir: &Path,
    uid: &str,
    version_fc: &str,
    dir_mime_id: i64,
    dir_mimepart_id: i64,
) -> Result<(), String> {
    // Split into segments: "files_versions/X/Y/file.v123"
    let segments: Vec<&str> = version_fc.split('/').collect();
    let propagator =
        crate::propagator::Propagator::new(pool.clone(), prefix.to_string(), storage_id);
    // We need all ancestors up to (but not including) the last segment.
    // The root "files_versions" may not exist yet.
    let mut built = String::new();

    for (i, seg) in segments.iter().enumerate() {
        // Stop before the last segment (the version file itself).
        if i + 1 >= segments.len() {
            break;
        }
        if i == 0 {
            built.push_str(seg);
        } else {
            built.push('/');
            built.push_str(seg);
        }

        // Check if this ancestor already exists.
        if crate::row::lookup_by_path(pool, prefix, storage_id, &built)
            .await
            .is_some()
        {
            continue;
        }

        // Determine parent fileid.
        let parent_fc = if i == 0 {
            // "files_versions" is a sibling of "files" — use the same parent
            // as "files" (i.e., the root entry with path "").
            match crate::row::lookup_by_path(pool, prefix, storage_id, "files").await {
                Some(files_row) => {
                    // Use files.parent so files_versions is a sibling.
                    if files_row.parent == files_row.fileid {
                        -1
                    } else {
                        files_row.parent
                    }
                }
                None => {
                    return Err("Cannot find root 'files' directory".to_string());
                }
            }
        } else {
            // Look up the immediate parent.
            crate::row::lookup_by_path(pool, prefix, storage_id, &{
                let mut parts: Vec<&str> = built.split('/').collect();
                parts.pop();
                parts.join("/")
            })
            .await
            .map(|r| r.fileid)
            .ok_or_else(|| format!("Parent not found for {built}"))?
        };

        let parent_fileid = parent_fc;

        let hash = row::path_hash(&built);
        let name = seg.to_string();
        let now = current_timestamp();

        let etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());

        let sql = format!(
            "INSERT INTO {prefix}filecache \
             (storage, path, path_hash, parent, name, mimetype, mimepart, \
              size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
             RETURNING fileid"
        );
        let _fid: i64 = match pool {
            DbPool::Pg(p) => sqlx::query_scalar::<sqlx::Postgres, _>(&sql)
                .bind(storage_id)
                .bind(&built)
                .bind(&hash)
                .bind(parent_fileid)
                .bind(&name)
                .bind(dir_mime_id)
                .bind(dir_mimepart_id)
                .bind(0i64)
                .bind(now)
                .bind(now)
                .bind(&etag)
                .bind(31i32)
                .bind("")
                .fetch_one(p)
                .await
                .map_err(|e| format!("insert ancestor {built}: {e}"))?,
            DbPool::Sqlite(p) => sqlx::query_scalar::<sqlx::Sqlite, _>(&sql)
                .bind(storage_id)
                .bind(&built)
                .bind(&hash)
                .bind(parent_fileid)
                .bind(&name)
                .bind(dir_mime_id)
                .bind(dir_mimepart_id)
                .bind(0i64)
                .bind(now)
                .bind(now)
                .bind(&etag)
                .bind(31i32)
                .bind("")
                .fetch_one(p)
                .await
                .map_err(|e| format!("insert ancestor {built}: {e}"))?,
        };

        // No oc_filecache_extended row — PHP's `View::mkdir` → `Cache::insert`
        // never writes extension fields for directories (same class as finding
        // #9); the version dirs' creation used to insert one, which was a
        // diff-visible divergence on `files_versions`.

        // PHP `View::mkdir` → `Updater::update()` side effects: the new dir's
        // parent gets `storage_mtime` = its disk mtime (creating
        // `files_versions`, a direct child of the storage root, stamps the
        // root's storage_mtime), and every ancestor gets one shared etag +
        // bumped mtime.  These etag stamps are transient (the copy's
        // propagation overwrites them) but the storage_mtime stamp is the
        // observable parity behavior.
        let parent_fc = {
            let mut parts: Vec<&str> = built.split('/').collect();
            parts.pop();
            parts.join("/")
        };
        let parent_disk = crate::row::disk_path(data_dir, uid, &parent_fc);
        if let Err(e) = propagator
            .correct_parent_storage_mtime(&parent_fc, &parent_disk)
            .await
        {
            warn!(dir = %built, error = %e, "ensure_version_parents: parent storage_mtime correction failed");
        }
        if let Err(e) = propagator.propagate_change(&built, now, 0).await {
            warn!(dir = %built, error = %e, "ensure_version_parents: mkdir propagation failed");
        }
    }

    Ok(())
}

/// Insert a filecache row for a saved version.
///
/// §9.4: inherits `permissions`, `creation_time`, and `upload_time` from the
/// overwritten source file so the PHP-FPM versions PROPFIND sees the same
/// metadata as the original (matching PHP's `View::copy()` behaviour).
///
/// `etag` is the SOURCE file's etag — PHP's `Cache::copyFromCache` clones the
/// source row as-is, so the version row carries the old content's etag
/// (live-verified against the oracle).  `storage_mtime` is the copied file's
/// disk mtime (the copy time — PHP `updateStorageMTimeOnly`).  `checksum`
/// stays NULL (the clone drops it), matching the oracle.
async fn insert_version_row(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    mime_cache: &SharedMimeCache,
    version_fc: &str,
    size: i64,
    mtime: i64,
    mimetype: i64,
    permissions: i32,
    creation_time: i64,
    upload_time: i64,
    etag: &str,
    storage_mtime: i64,
) {
    let hash = row::path_hash(version_fc);
    let name = version_fc.rsplit('/').next().unwrap_or("").to_string();

    // Look up the immediate parent (must exist after ensure_version_parents).
    let parent_fc = {
        let mut parts: Vec<&str> = version_fc.split('/').collect();
        parts.pop();
        parts.join("/")
    };
    let parent_id = match row::lookup_by_path(pool, prefix, storage_id, &parent_fc).await {
        Some(r) => r.fileid,
        None => {
            warn!("insert_version_row: parent not found for {version_fc}");
            return;
        }
    };

    let mimepart_id = {
        let part_str = {
            let cache = mime_cache.read().expect("mime cache lock");
            let mime_arc = cache
                .get_name(mimetype)
                .unwrap_or_else(|| Arc::from("application/octet-stream"));
            let mime_str = mime_arc.as_ref();
            mime_str
                .split('/')
                .next()
                .unwrap_or("application")
                .to_string()
        };
        // Drop the RwLockReadGuard before the await below.
        nc_db::mime::get_or_insert_mime_id(pool, prefix, mime_cache, &part_str).await
    };

    // Check if a row already exists for this version path.
    if row::lookup_by_path(pool, prefix, storage_id, version_fc)
        .await
        .is_some()
    {
        return;
    }

    // `checksum` is intentionally not bound: PHP's copy-clone row leaves it
    // NULL (the oracle shows NULL, not '').
    let sql = format!(
        "INSERT INTO {prefix}filecache \
         (storage, path, path_hash, parent, name, mimetype, mimepart, \
          size, mtime, storage_mtime, etag, permissions) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
         RETURNING fileid"
    );
    let fetched: Result<i64, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query_scalar::<sqlx::Postgres, _>(&sql)
            .bind(storage_id)
            .bind(version_fc)
            .bind(&hash)
            .bind(parent_id)
            .bind(&name)
            .bind(mimetype)
            .bind(mimepart_id)
            .bind(size)
            .bind(mtime)
            .bind(storage_mtime)
            .bind(etag)
            .bind(permissions)
            .fetch_one(p)
            .await,
        DbPool::Sqlite(p) => sqlx::query_scalar::<sqlx::Sqlite, _>(&sql)
            .bind(storage_id)
            .bind(version_fc)
            .bind(&hash)
            .bind(parent_id)
            .bind(&name)
            .bind(mimetype)
            .bind(mimepart_id)
            .bind(size)
            .bind(mtime)
            .bind(storage_mtime)
            .bind(etag)
            .bind(permissions)
            .fetch_one(p)
            .await,
    };
    let fid: i64 = match fetched {
        Ok(id) => id,
        Err(e) => {
            warn!("insert_version_row: INSERT failed for {version_fc}: {e}");
            return;
        }
    };

    // Insert extended row with inherited creation_time and upload_time.
    let sql_ext = format!(
        "INSERT INTO {prefix}filecache_extended \
         (fileid, metadata_etag, creation_time, upload_time) \
         VALUES ($1, '', $2, $3) \
         ON CONFLICT(fileid) DO NOTHING"
    );
    let _ = match pool {
        DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&sql_ext)
            .bind(fid)
            .bind(creation_time)
            .bind(upload_time)
            .execute(p)
            .await
            .map(|_| ()),
        DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&sql_ext)
            .bind(fid)
            .bind(creation_time)
            .bind(upload_time)
            .execute(p)
            .await
            .map(|_| ()),
    };
}

/// Insert a row into `oc_files_versions` so PHP-FPM's version PROPFIND can
/// serve `nc:version-author` and other metadata.
///
/// Matches PHP `NodeCreatedEvent` → `createVersionEntity()` + `NodeWrittenEvent`
/// → `VersionAuthorListener::post_write_hook()`. Called after every successful
/// write (new file or overwrite) with the file's current mtime/size/mimetype.
///
/// Columns: `file_id` = file id, `timestamp` = the file's mtime,
/// `size` = file size, `mimetype` = file mimetype id,
/// `metadata` = `{"author": uid}` (JSON).
pub(crate) async fn insert_version_entity(
    pool: &DbPool,
    prefix: &str,
    source_fileid: i64,
    timestamp: i64,
    size: i64,
    mimetype: i64,
    author_uid: &str,
) {
    // Build metadata JSON matching PHP `json_encode(['author' => $uid])` — compact,
    // no space after the colon (see finding #4 / phase-16.4).
    let metadata_json = version_metadata_json(author_uid);

    // PHP `createVersionEntity` (LegacyVersionsBackend.php:232-254) retries up
    // to 5 times on a constraint violation, bumping the timestamp by 1 — a
    // same-second overwrite collides on the unique (file_id, timestamp) key,
    // and the retried insert must land so the entity reflects the CURRENT
    // file state (live-verified: the oracle ends with an entity sized as the
    // post-write file, the SUT's plain insert silently dropped it).
    let mut ts = timestamp;
    for attempt in 0..5 {
        let insert_sql = format!(
            "INSERT INTO {prefix}files_versions (file_id, \"timestamp\", size, mimetype, metadata) \
             VALUES ($1, $2, $3, $4, $5::json)"
        );
        let result = match pool {
            DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&insert_sql)
                .bind(source_fileid)
                .bind(ts)
                .bind(size)
                .bind(mimetype)
                .bind(&metadata_json)
                .execute(p)
                .await
                .map(|_| ()),
            DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&insert_sql)
                .bind(source_fileid)
                .bind(ts)
                .bind(size)
                .bind(mimetype)
                .bind(&metadata_json)
                .execute(p)
                .await
                .map(|_| ()),
        };
        match result {
            Ok(()) => return,
            Err(e) => {
                let msg = e.to_string();
                if attempt + 1 >= 5 {
                    warn!(source_fileid, timestamp, error = %e, "Failed to insert oc_files_versions row");
                    return;
                }
                if msg.contains("UNIQUE")
                    || msg.contains("unique")
                    || msg.contains("constraint")
                    || msg.contains("duplicate")
                {
                    ts += 1;
                    continue;
                }
                warn!(source_fileid, timestamp, error = %e, "Failed to insert oc_files_versions row");
                return;
            }
        }
    }
}

/// Build the `oc_files_versions.metadata` JSON for a write by `author_uid`.
///
/// Must match PHP `json_encode(['author' => $uid])` exactly — the compact form
/// with **no space** after the colon (`VersionEntity` maps `metadata` to
/// `Types::JSON`, serialized by `json_encode`). A space here is a real,
/// diff-visible divergence (finding #4 / phase-16.4). `serde_json`'s default
/// `to_string` is compact and escapes the same characters PHP does.
pub(crate) fn version_metadata_json(author_uid: &str) -> String {
    serde_json::json!({ "author": author_uid }).to_string()
}

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, RwLock};

    use nc_db::mime::MimeCache;

    /// The in-memory test DB is always SQLite; unwrap the variant for the
    /// native queries below (tests never construct a Pg pool).
    fn test_pool(pool: &DbPool) -> &sqlx::SqlitePool {
        match pool {
            DbPool::Sqlite(p) => p,
            DbPool::Pg(_) => panic!("test pools are sqlite"),
        }
    }

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn fresh_data_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nc-dav-versions-test-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("admin/files")).unwrap();
        dir
    }

    /// In-memory SQLite with the versioning tables and a seeded home tree
    /// (fileids matching the nc-dav test convention):
    /// ```text
    /// 1  "" (root)  2  "files"  4  "files/hello.txt" (26, etag "old-etag", mtime 100)
    /// ```
    async fn fresh_db() -> (DbPool, String, i64) {
        let pool = DbPool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("in-memory SQLite"),
        );

        sqlx::query::<sqlx::Sqlite>(
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
        .execute(test_pool(&pool))
        .await
        .expect("create filecache");
        sqlx::query::<sqlx::Sqlite>(
            "CREATE TABLE oc_filecache_extended (
                fileid         INTEGER NOT NULL PRIMARY KEY,
                metadata_etag  VARCHAR(40) NOT NULL DEFAULT '',
                creation_time  BIGINT NOT NULL DEFAULT 0,
                upload_time    BIGINT NOT NULL DEFAULT 0
            )",
        )
        .execute(test_pool(&pool))
        .await
        .expect("create filecache_extended");
        // The unique (file_id, timestamp) key mirrors the production schema —
        // the same-second overwrite conflict exercises the retry loop.
        sqlx::query::<sqlx::Sqlite>(
            "CREATE TABLE oc_files_versions (
                id         INTEGER NOT NULL PRIMARY KEY,
                file_id    BIGINT NOT NULL,
                \"timestamp\" BIGINT NOT NULL,
                size       BIGINT NOT NULL,
                mimetype   BIGINT NOT NULL,
                metadata   TEXT,
                UNIQUE (file_id, \"timestamp\")
            )",
        )
        .execute(test_pool(&pool))
        .await
        .expect("create files_versions");
        sqlx::query::<sqlx::Sqlite>(
            "CREATE TABLE oc_mimetypes (
                id       BIGINT NOT NULL PRIMARY KEY,
                mimetype VARCHAR(255) NOT NULL
            )",
        )
        .execute(test_pool(&pool))
        .await
        .expect("create mimetypes");

        let prefix = "oc_".to_string();
        let storage_id = 1i64;
        for (fid, path, parent, size, name, etag) in [
            (1i64, "", -1i64, -1i64, "", "root-etag"),
            (2, "files", 1, 100, "files", "files-etag"),
            (4, "files/hello.txt", 2, 26, "hello.txt", "old-etag"),
        ] {
            sqlx::query::<sqlx::Sqlite>(&format!(
                "INSERT INTO {prefix}filecache \
                 (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
                  size, mtime, storage_mtime, etag, permissions, checksum) \
                 VALUES ($1, $2, $3, $4, $5, $6, 0, 0, $7, 100, 100, $8, 27, '')"
            ))
            .bind(fid)
            .bind(storage_id)
            .bind(path)
            .bind(row::path_hash(path))
            .bind(parent)
            .bind(name)
            .bind(size)
            .bind(etag)
            .execute(test_pool(&pool))
            .await
            .expect("seed filecache");
        }

        (pool, prefix, storage_id)
    }

    #[test]
    fn version_metadata_json_is_compact() {
        // PHP json_encode(['author' => 'admin']) -> {"author":"admin"}
        assert_eq!(version_metadata_json("admin"), r#"{"author":"admin"}"#);
    }

    #[test]
    fn version_metadata_json_escapes_special_chars() {
        // json_encode escapes quotes/backslashes/control chars; serde_json matches.
        assert_eq!(version_metadata_json(r#"a"b"#), r#"{"author":"a\"b"}"#);
    }

    /// The version row is a clone of the overwritten file (PHP `View::copy` →
    /// `Cache::copyFromCache`): it carries the SOURCE etag, the source mtime,
    /// a NULL checksum, and `storage_mtime` = the copy time; the parent dir
    /// gets its size recomputed, its mtime/etag propagated, and NO extended
    /// row.
    #[tokio::test]
    async fn store_version_inherits_etag_and_updates_parent_dir() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let data_dir = fresh_data_dir();
        let disk = data_dir.join("admin/files/hello.txt");
        std::fs::write(&disk, vec![b'x'; 26]).unwrap();
        let mime_cache: SharedMimeCache = Arc::new(RwLock::new(MimeCache::default()));

        store_version(
            &pool,
            &prefix,
            &data_dir,
            "admin",
            storage_id,
            &mime_cache,
            "files/hello.txt",
            &disk,
            26,  // old_size
            100, // old_mtime
            0,   // old_mimetype
            27,  // old_permissions
            0,   // old_creation_time
            0,   // old_upload_time
            4,   // source_fileid
            "old-etag",
            2, // dir_mime_id (fixture: mimetype 2 = directory)
            1, // dir_mimepart_id
        )
        .await;

        // The version row inherits the source's etag and mtime; checksum NULL;
        // storage_mtime = the copy time (now, not the old mtime).
        let v = row::lookup_by_path(&pool, &prefix, storage_id, "files_versions/hello.txt.v100")
            .await
            .expect("version row");
        assert_eq!(
            v.etag.as_deref(),
            Some("old-etag"),
            "version row must inherit the source etag"
        );
        assert_eq!(v.mtime, 100, "version row must keep the source mtime");
        assert_eq!(v.size, 26);
        assert!(
            v.storage_mtime >= 100,
            "storage_mtime {} should be the copy time (now), not the old mtime",
            v.storage_mtime
        );
        let checksum: Option<String> = sqlx::query_scalar::<sqlx::Sqlite, _>(&format!(
            "SELECT checksum FROM {prefix}filecache WHERE fileid = $1"
        ))
        .bind(v.fileid)
        .fetch_one(test_pool(&pool))
        .await
        .unwrap();
        assert_eq!(checksum, None, "version row checksum must be NULL");

        // The parent dir: size recomputed from children, mtime/etag propagated,
        // and no extended row (PHP dirs never have one).
        let dir = row::lookup_by_path(&pool, &prefix, storage_id, "files_versions")
            .await
            .expect("files_versions dir");
        assert_eq!(
            dir.size, 26,
            "files_versions dir must gain the version file's size"
        );
        assert!(
            dir.mtime >= 100,
            "files_versions dir mtime {} should be bumped by the propagation",
            dir.mtime
        );
        let dir_ext: i64 =
            sqlx::query_scalar::<sqlx::Sqlite, _>("SELECT COUNT(*) FROM oc_filecache_extended WHERE fileid = $1")
                .bind(dir.fileid)
                .fetch_one(test_pool(&pool))
                .await
                .unwrap();
        assert_eq!(dir_ext, 0, "version dirs must have no extended rows");
    }

    /// A same-second overwrite collides on the unique (file_id, timestamp)
    /// key; the insert must bump the timestamp and retry (PHP
    /// `createVersionEntity`'s 5-try loop) so the newest entity reflects the
    /// current file state.
    #[tokio::test]
    async fn insert_version_entity_bumps_timestamp_on_conflict() {
        let (pool, prefix, _) = fresh_db().await;
        sqlx::query::<sqlx::Sqlite>(
            "INSERT INTO oc_files_versions (file_id, \"timestamp\", size, mimetype, metadata) \
             VALUES (4, 100, 26, 0, '{\"author\":\"admin\"}')",
        )
        .execute(test_pool(&pool))
        .await
        .unwrap();

        insert_version_entity(&pool, &prefix, 4, 100, 31, 0, "admin").await;

        let rows: Vec<(i64, i64)> = sqlx::query_as::<sqlx::Sqlite, (i64, i64)>(
            "SELECT \"timestamp\", size FROM oc_files_versions WHERE file_id = 4",
        )
        .fetch_all(test_pool(&pool))
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "the retried insert must land a second row");
        let (ts, size) = rows.iter().max().copied().unwrap();
        assert!(ts > 100, "the new entity must carry a bumped timestamp");
        assert_eq!(
            size, 31,
            "the new entity must reflect the post-write file size"
        );
    }

    #[tokio::test]
    async fn rename_versions_repaths_directory_subtree() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let data_dir = fresh_data_dir();
        // On-disk version subtree under files_versions/Photos/2024/.
        let vdir = data_dir.join("admin/files_versions/Photos/2024");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("photo.jpg.v100"), vec![b'x'; 26]).unwrap();

        // Seed the version filecache subtree.
        for (fid, path, parent, name) in [
            (6i64, "files_versions", 1i64, "files_versions"),
            (7, "files_versions/Photos", 6, "Photos"),
            (8, "files_versions/Photos/2024", 7, "2024"),
            (9, "files_versions/Photos/2024/photo.jpg.v100", 8, "photo.jpg.v100"),
        ] {
            sqlx::query::<sqlx::Sqlite>(&format!(
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
            .bind(26i64)
            .execute(test_pool(&pool))
            .await
            .unwrap();
        }

        rename_versions(
            &pool,
            &prefix,
            &data_dir,
            "admin",
            storage_id,
            "files/Photos",
            "files/Photos2",
        )
        .await;

        // Old subtree is gone from the cache.
        assert!(
            row::lookup_by_path(&pool, &prefix, storage_id, "files_versions/Photos")
                .await
                .is_none(),
            "old files_versions/Photos subtree must be repathed away"
        );
        // The moved directory node was repathed, with its name rewritten.
        let photos2 = row::lookup_by_path(&pool, &prefix, storage_id, "files_versions/Photos2")
            .await
            .expect("moved version dir must exist");
        assert_eq!(photos2.name.as_deref(), Some("Photos2"));
        // PHP `Cache::move` recomputes the moved node's parent: it must point
        // at the new parent dir (files_versions root = id 6), not the old one.
        assert_eq!(
            photos2.parent, 6,
            "moved version dir's parent must be repointed to its new parent"
        );
        // An intermediate dir and the deep version file followed the move,
        // keeping their fileids.
        assert!(
            row::lookup_by_path(&pool, &prefix, storage_id, "files_versions/Photos2/2024")
                .await
                .is_some(),
            "intermediate version dir must follow the rename"
        );
        let v = row::lookup_by_path(
            &pool,
            &prefix,
            storage_id,
            "files_versions/Photos2/2024/photo.jpg.v100",
        )
        .await
        .expect("version file row must follow the rename");
        assert_eq!(v.fileid, 9, "fileid must be preserved across the move");
        assert_eq!(v.name.as_deref(), Some("photo.jpg.v100"));
    }


    #[tokio::test]
    async fn copy_versions_clones_version_row() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let data_dir = fresh_data_dir();
        // On-disk version file for files/hello.txt + the files_versions dir.
        let vdir = data_dir.join("admin/files_versions");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("hello.txt.v100"), vec![b'x'; 26]).unwrap();

        // Seed files_versions (id 6) and the source version file row (id 7).
        for (fid, path, parent, name) in [
            (6i64, "files_versions", 1i64, "files_versions"),
            (7, "files_versions/hello.txt.v100", 6, "hello.txt.v100"),
        ] {
            sqlx::query::<sqlx::Sqlite>(&format!(
                "INSERT INTO {prefix}filecache \
                 (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
                  size, mtime, storage_mtime, etag, permissions, checksum) \
                 VALUES ($1, $2, $3, $4, $5, $6, 0, 0, $7, 100, 100, 'src-etag', 27, '')"
            ))
            .bind(fid)
            .bind(storage_id)
            .bind(path)
            .bind(row::path_hash(path))
            .bind(parent)
            .bind(name)
            .bind(26i64)
            .execute(test_pool(&pool))
            .await
            .unwrap();
        }

        copy_versions(
            &pool,
            &prefix,
            &data_dir,
            "admin",
            storage_id,
            "files/hello.txt",
            "files/hello2.txt",
        )
        .await;

        // A NEW row (new fileid, not id 7) must exist for the copy, with the
        // cloned etag and correct parent.
        let src = row::lookup_by_path(&pool, &prefix, storage_id, "files_versions/hello.txt.v100")
            .await
            .expect("source version row");
        let copy =
            row::lookup_by_path(&pool, &prefix, storage_id, "files_versions/hello2.txt.v100")
                .await
                .expect("copied version row must exist");
        assert_ne!(copy.fileid, src.fileid, "copy must get a brand-new fileid");
        assert_eq!(copy.parent, 6, "copied row's parent is files_versions root");
        assert_eq!(copy.size, 26);
        assert_eq!(copy.mtime, 100);
        assert_eq!(copy.etag.as_deref(), Some("src-etag"), "clone must keep the source etag");
        assert!(data_dir.join("admin/files_versions/hello2.txt.v100").exists());
    }

}
