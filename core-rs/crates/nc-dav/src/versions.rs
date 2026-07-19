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

use std::path::Path;

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
) {
    debug!(
        fc_path,
        old_size,
        old_mtime,
        old_mimetype,
        source_fileid,
        "store_version called"
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
        if cache.get_name(old_mimetype) == Some("httpd/unix-directory") {
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
    if let Err(e) = ensure_version_parents(pool, prefix, storage_id, mime_cache, &version_fc).await {
        warn!("store_version: ensure_version_parents failed: {e}");
        return;
    }

    // Insert a filecache row for the version so the PHP-FPM versions
    // PROPFIND can enumerate it (PHP LegacyVersionsBackend::createVersion:160-162).
    // §9.4: inherit permissions, creation_time, and upload_time from the source file.
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
    )
    .await;
}

/// Relocate versions when a file is renamed or moved.
///
/// For files: moves all `files_versions/{oldRel}.v*` to `files_versions/{newRel}.v*`.
/// For directories: moves the entire `files_versions/{oldRel}/` subtree.
pub async fn rename_versions(
    _pool: &DbPool,
    _prefix: &str,
    data_dir: &Path,
    uid: &str,
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

    let old_versions_dir = crate::row::disk_path(data_dir, uid, &format!("files_versions/{old_rel}"));
    let new_versions_dir = crate::row::disk_path(data_dir, uid, &format!("files_versions/{new_rel}"));

    // Check if the old path is a directory or a file (by checking disk).
    if old_versions_dir.is_dir() {
        // Directory: move the whole versions subdirectory.
        if let Some(parent) = new_versions_dir.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(e) = tokio::fs::rename(&old_versions_dir, &new_versions_dir).await {
            warn!("rename_versions: directory rename failed {old_versions_dir:?} → {new_versions_dir:?}: {e}");
        }
    } else {
        // File: find and rename individual version files matching .v{ts}.
        if let Some(parent) = old_versions_dir.parent() {
            let prefix_pattern = old_versions_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Ok(mut entries) = tokio::fs::read_dir(parent).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with(&prefix_pattern)
                        && name_str[prefix_pattern.len()..].starts_with(".v")
                    {
                        let new_name =
                            format!("{}{}", new_rel, &name_str[prefix_pattern.len()..]);
                        let new_disk =
                            crate::row::disk_path(data_dir, uid, &format!("files_versions/{new_name}"));
                        if let Some(np) = new_disk.parent() {
                            let _ = tokio::fs::create_dir_all(np).await;
                        }
                        if let Err(e) = tokio::fs::rename(&entry.path(), &new_disk).await {
                            warn!("rename_versions: file rename failed {name_str}: {e}");
                        }
                    }
                }
            }
        }
    }
}

/// Copy versions when a file is copied.
///
/// For files: copies all `files_versions/{oldRel}.v*` to `files_versions/{newRel}.v*`.
pub async fn copy_versions(
    _pool: &DbPool,
    _prefix: &str,
    data_dir: &Path,
    uid: &str,
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

    let old_versions_dir = crate::row::disk_path(data_dir, uid, &format!("files_versions/{old_rel}"));
    let parent_of_versions = old_versions_dir.parent();
    let base_name = old_versions_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if let Some(parent) = parent_of_versions {
        if let Ok(mut entries) = tokio::fs::read_dir(parent).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with(&base_name)
                    && name_str[base_name.len()..].starts_with(".v")
                {
                    let new_name = format!("{}{}", new_rel, &name_str[base_name.len()..]);
                    let new_disk =
                        crate::row::disk_path(data_dir, uid, &format!("files_versions/{new_name}"));
                    if let Some(np) = new_disk.parent() {
                        let _ = tokio::fs::create_dir_all(np).await;
                    }
                    if let Err(e) = tokio::fs::copy(&entry.path(), &new_disk).await {
                        warn!("copy_versions: file copy failed {name_str}: {e}");
                    }
                }
            }
        }
    }
}

// ─── Internal helpers ──────────────────────────────────────────────────────────

/// Ensure the parent chain for a version path exists in filecache.
async fn ensure_version_parents(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    mime_cache: &SharedMimeCache,
    version_fc: &str,
) -> Result<(), String> {
    // Split into segments: "files_versions/X/Y/file.v123"
    let segments: Vec<&str> = version_fc.split('/').collect();
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

        let dir_mime_id = nc_db::mime::get_or_insert_mime_id(
            pool,
            prefix,
            mime_cache,
            "httpd/unix-directory",
        )
        .await;
        let dir_mimepart_id = nc_db::mime::get_or_insert_mime_id(
            pool,
            prefix,
            mime_cache,
            "httpd",
        )
        .await;
        let etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());

        let sql = format!(
            "INSERT INTO {prefix}filecache \
             (storage, path, path_hash, parent, name, mimetype, mimepart, \
              size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
             RETURNING fileid"
        );
        let fid: i64 = sqlx::query_scalar(&sql)
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
            .fetch_one(pool)
            .await
            .map_err(|e| format!("insert ancestor {built}: {e}"))?;

        // Insert extended row.
        let sql_ext = format!(
            "INSERT INTO {prefix}filecache_extended \
             (fileid, metadata_etag, creation_time, upload_time) \
             VALUES ($1, '', $2, $2) \
             ON CONFLICT(fileid) DO NOTHING"
        );
        let _ = sqlx::query(&sql_ext)
            .bind(fid)
            .bind(now)
            .execute(pool)
            .await;
    }

    Ok(())
}

/// Insert a filecache row for a saved version.
///
/// §9.4: inherits `permissions`, `creation_time`, and `upload_time` from the
/// overwritten source file so the PHP-FPM versions PROPFIND sees the same
/// metadata as the original (matching PHP's `View::copy()` behaviour).
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

    let etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());

    let mimepart_id = {
        let part_str = {
            let cache = mime_cache.read().expect("mime cache lock");
            let mime_str = cache
                .get_name(mimetype)
                .unwrap_or("application/octet-stream");
            mime_str.split('/').next().unwrap_or("application").to_string()
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

    let sql = format!(
        "INSERT INTO {prefix}filecache \
         (storage, path, path_hash, parent, name, mimetype, mimepart, \
          size, mtime, storage_mtime, etag, permissions, checksum) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
         RETURNING fileid"
    );
    let fid: i64 = match sqlx::query_scalar(&sql)
        .bind(storage_id)
        .bind(version_fc)
        .bind(&hash)
        .bind(parent_id)
        .bind(&name)
        .bind(mimetype)
        .bind(mimepart_id)
        .bind(size)
        .bind(mtime)
        .bind(mtime)
        .bind(&etag)
        .bind(permissions)
        .bind("")
        .fetch_one(pool)
        .await
    {
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
    let _ = sqlx::query(&sql_ext)
        .bind(fid)
        .bind(creation_time)
        .bind(upload_time)
        .execute(pool)
        .await;
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
    // Build metadata JSON: {"author": "uid"}
    let metadata_json = format!("{{\"author\": \"{author_uid}\"}}");

    let insert_sql = format!(
        "INSERT INTO {prefix}files_versions (file_id, \"timestamp\", size, mimetype, metadata) \
         VALUES ($1, $2, $3, $4, $5::json)"
    );
    if let Err(e) = sqlx::query(&insert_sql)
        .bind(source_fileid)
        .bind(timestamp)
        .bind(size)
        .bind(mimetype)
        .bind(&metadata_json)
        .execute(pool)
        .await
    {
        warn!(source_fileid, timestamp, error = %e, "Failed to insert oc_files_versions row");
    }
}

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
