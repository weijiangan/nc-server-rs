//! Low-level database helpers for `oc_filecache`, `oc_filecache_extended`,
//! and `oc_storages`.
//!
//! All queries are parameterised and use the table prefix from `NcDavState`.

use md5::{Digest, Md5};
use nc_db::pool::DbPool;
use sqlx::Row;

// ─── Types ────────────────────────────────────────────────────────────────────

/// One row from `oc_filecache`.
#[derive(Debug, Clone)]
pub struct FileCacheRow {
    pub fileid: i64,
    pub storage: i64,
    pub path: Option<String>,
    pub path_hash: String,
    pub parent: i64,
    pub name: Option<String>,
    pub mimetype: i64,
    pub mimepart: i64,
    pub size: i64,
    pub mtime: i64,
    pub storage_mtime: i64,
    pub etag: Option<String>,
    pub permissions: i32,
    pub checksum: Option<String>,
    pub creation_time: i64,
    pub upload_time: i64,
}

/// Extended row from `oc_filecache_extended` (authoritative for times, REQ §9.4).
#[derive(Debug, Clone, Default)]
pub struct FileCacheExtRow {
    pub metadata_etag: Option<String>,
    pub creation_time: i64,
    pub upload_time: i64,
}

// ─── Path helpers ─────────────────────────────────────────────────────────────

/// Convert a WebDAV path (e.g. `/Photos/img.jpg` or `/`) to the path stored in
/// `oc_filecache` (e.g. `files/Photos/img.jpg` or `files`).
///
/// Nextcloud's home storage keeps all user files under a `files/` prefix.
pub fn dav_to_fc_path(dav_path: &str) -> String {
    let trimmed = dav_path.trim_matches('/');
    if trimmed.is_empty() {
        "files".to_string()
    } else {
        format!("files/{trimmed}")
    }
}

/// Compute the MD5 path hash used by `oc_filecache.path_hash`.
pub fn path_hash(path: &str) -> String {
    format!("{:x}", Md5::digest(path.as_bytes()))
}

/// Derive the disk path for a filecache entry on a local home storage.
///
/// Layout: `{data_directory}/{uid}/{fc_path}`
/// where `fc_path` already has the `files/` prefix (e.g. `files/Photos/img.jpg`).
pub fn disk_path(data_dir: &std::path::Path, uid: &str, fc_path: &str) -> std::path::PathBuf {
    data_dir.join(uid).join(fc_path)
}

// ─── Storage ─────────────────────────────────────────────────────────────────

/// Look up the `numeric_id` for a user's home storage.
///
/// Tries multiple known formats:
/// - `home::{uid}` — used by NC home directory wrappers (default for most installs)
/// - `local::{data_dir}/{uid}/` — used by raw LocalStorage backends
pub async fn lookup_storage_id(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    data_dir: &str,
) -> Option<i64> {
    let candidates = [
        format!("home::{uid}"),
        format!("local::{data_dir}/{uid}/"),
        format!("local::{data_dir}{uid}/"),
    ];
    let sql = format!("SELECT numeric_id FROM {prefix}storages WHERE id = $1");
    for key in &candidates {
        if let Some(row) = sqlx::query(&sql)
            .bind(key)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
        {
            return Some(row.get::<i64, _>("numeric_id"));
        }
    }
    None
}

// ─── Filecache queries ────────────────────────────────────────────────────────

/// Look up one filecache row by `storage` + path.
pub async fn lookup_by_path(
    pool: &DbPool,
    prefix: &str,
    storage: i64,
    path: &str,
) -> Option<FileCacheRow> {
    let hash = path_hash(path);
    let sql = format!(
        "SELECT fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
         size, mtime, storage_mtime, etag, permissions, checksum \
         FROM {prefix}filecache WHERE storage = $1 AND path_hash = $2"
    );
    let result = sqlx::query(&sql)
        .bind(storage)
        .bind(&hash)
        .fetch_optional(pool)
        .await;
    match &result {
        Err(e) => {
            tracing::error!(error = %e, path = %path, hash = %hash, storage, "lookup_by_path: SQL error");
        }
        Ok(Some(row)) => {
            let db_path: Option<String> = row.get("path");
            let db_storage: i64 = row.get("storage");
            tracing::info!(path = %path, hash = %hash, storage, db_path = ?db_path, db_storage, "lookup_by_path: found");
        }
        Ok(None) => {
            // Debug: query without the storage filter to check if path_hash
            // matches any row at all.
            let debug_sql = format!(
                "SELECT fileid, storage, path FROM {prefix}filecache WHERE path_hash = $1",
                prefix = prefix
            );
            let debug_rows: Vec<(i64, i64, Option<String>)> = sqlx::query(&debug_sql)
                .bind(&hash)
                .fetch_all(pool)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.get(0), r.get(1), r.get(2)))
                .collect();
            tracing::info!(
                path = %path, hash = %hash, storage, ?debug_rows,
                "lookup_by_path: not found (any storage)"
            );
        }
    }
    match result {
        Err(_) => None,
        Ok(row) => row.map(|r| fc_row_from_any(&r)),
    }
}

/// Look up one filecache row by its `fileid`.
pub async fn lookup_by_id(pool: &DbPool, prefix: &str, fileid: i64) -> Option<FileCacheRow> {
    let sql = format!(
        "SELECT fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
         size, mtime, storage_mtime, etag, permissions, checksum \
         FROM {prefix}filecache WHERE fileid = $1"
    );
    match sqlx::query(&sql).bind(fileid).fetch_optional(pool).await {
        Err(e) => {
            tracing::error!(error = %e, fileid = fileid, "lookup_by_id: SQL error");
            None
        }
        Ok(row) => row.map(|r| fc_row_from_any(&r)),
    }
}

/// Fetch all direct children of `parent_id` in the given storage.
pub async fn list_children(
    pool: &DbPool,
    prefix: &str,
    parent_id: i64,
    storage: i64,
) -> Vec<FileCacheRow> {
    let sql = format!(
        "SELECT fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
         size, mtime, storage_mtime, etag, permissions, checksum \
         FROM {prefix}filecache WHERE parent = $1 AND storage = $2"
    );
    match sqlx::query(&sql)
        .bind(parent_id)
        .bind(storage)
        .fetch_all(pool)
        .await
    {
        Err(e) => {
            tracing::error!(error = %e, parent_id = parent_id, "list_children: SQL error");
            Vec::new()
        }
        Ok(rows) => rows.iter().map(fc_row_from_any).collect(),
    }
}

/// Fetch extended metadata for a file (creation_time, upload_time, metadata_etag).
pub async fn get_extended(pool: &DbPool, prefix: &str, fileid: i64) -> FileCacheExtRow {
    let sql = format!(
        "SELECT metadata_etag, creation_time, upload_time \
         FROM {prefix}filecache_extended WHERE fileid = $1"
    );
    sqlx::query(&sql)
        .bind(fileid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(|r| FileCacheExtRow {
            metadata_etag: r.get("metadata_etag"),
            creation_time: r.get::<i64, _>("creation_time"),
            upload_time: r.get::<i64, _>("upload_time"),
        })
        .unwrap_or_default()
}

/// Fetch extended metadata for a **batch** of files in a single query.
///
/// Returns a `HashMap<fileid, FileCacheExtRow>`.  Files with no extended row
/// are absent from the map; callers should fall back to zero values for them.
///
/// Used by `read_dir` so that depth-1 PROPFIND returns correct
/// `{nc:}creation_time`, `{nc:}upload_time`, and `{nc:}metadata_etag` without
/// issuing one query per child (REQ §4.1 Phase-4 tracker).
pub async fn list_extended_batch(
    pool: &DbPool,
    prefix: &str,
    fileids: &[i64],
) -> std::collections::HashMap<i64, FileCacheExtRow> {
    if fileids.is_empty() {
        return std::collections::HashMap::new();
    }

    // Build an IN clause with numbered $N placeholders per fileid.
    let placeholders = (1..=fileids.len())
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT fileid, metadata_etag, creation_time, upload_time \
         FROM {prefix}filecache_extended WHERE fileid IN ({placeholders})"
    );

    let mut query = sqlx::query(&sql);
    for id in fileids {
        query = query.bind(*id);
    }

    query
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let fileid: i64 = r.get("fileid");
            let ext = FileCacheExtRow {
                metadata_etag: r.get("metadata_etag"),
                creation_time: r.get::<i64, _>("creation_time"),
                upload_time: r.get::<i64, _>("upload_time"),
            };
            (fileid, ext)
        })
        .collect()
}

/// Count direct children of a directory, split into (dir_count, file_count).
///
/// Used to populate `{nc:}contained-folder-count` and `{nc:}contained-file-count`.
pub async fn count_children(
    pool: &DbPool,
    prefix: &str,
    parent_id: i64,
    storage: i64,
    dir_mimetype_id: i64,
) -> (i64, i64) {
    use sqlx::Row as _;
    let sql = format!(
        "SELECT \
         SUM(CASE WHEN mimetype = $1 THEN 1 ELSE 0 END) AS dirs, \
         SUM(CASE WHEN mimetype != $2 THEN 1 ELSE 0 END) AS files \
         FROM {prefix}filecache WHERE parent = $3 AND storage = $4"
    );
    let row = sqlx::query(&sql)
        .bind(dir_mimetype_id)
        .bind(dir_mimetype_id)
        .bind(parent_id)
        .bind(storage)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    match row {
        Some(r) => {
            let dirs: i64 = r.get::<Option<i64>, _>("dirs").unwrap_or(0);
            let files: i64 = r.get::<Option<i64>, _>("files").unwrap_or(0);
            (dirs, files)
        }
        None => (0, 0),
    }
}

/// Look up the display name for a user from `oc_users.displayname`.
///
/// Falls back to returning `uid` if no row exists or the `displayname` column
/// is NULL or empty.  This is used for `{oc:}owner-display-name` (REQ §6.5).
pub async fn lookup_user_display_name(pool: &DbPool, prefix: &str, uid: &str) -> String {
    let sql = format!("SELECT displayname FROM {prefix}users WHERE uid = $1");
    sqlx::query(&sql)
        .bind(uid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.get::<Option<String>, _>("displayname"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uid.to_string())
}

/// Look up the string ID for a storage row by its numeric ID.
///
/// Used in `get_props()` to determine whether a file's storage is a home
/// storage (`id` starts with `"home::"`) for the `M` (mounted) flag in
/// `{oc:}permissions` (PHASE-7.6).  Returns `None` when the storage row does
/// not exist.
pub async fn get_storage_string_id(pool: &DbPool, prefix: &str, numeric_id: i64) -> Option<String> {
    let sql = format!("SELECT id FROM {prefix}storages WHERE numeric_id = $1");
    sqlx::query_scalar::<_, Option<String>>(&sql)
        .bind(numeric_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
}

/// Return the MAX permissions from `oc_share` for a given file and owner/initiator.
///
/// Query: `SELECT MAX(permissions) FROM oc_share WHERE (uid_owner = ? OR
/// uid_initiator = ?) AND file_source = ? AND share_type IN (0,1,3)`.
///
/// Returns `31` (all permissions) when the file has no share rows, which
/// represents the owner's own unshared file (REQ §6.5, PHASE-7.6).
pub async fn get_share_max_permissions(pool: &DbPool, prefix: &str, uid: &str, fileid: i64) -> i32 {
    let sql = format!(
        "SELECT MAX(permissions) FROM {prefix}share \
         WHERE (uid_owner = $1 OR uid_initiator = $2) AND file_source = $3 \
         AND share_type IN (0,1,3)"
    );
    sqlx::query_scalar::<_, Option<i32>>(&sql)
        .bind(uid)
        .bind(uid)
        .bind(fileid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or(31)
}

/// Return the most recent non-empty share note for a file.
///
/// Query: `SELECT note FROM oc_share WHERE file_source = ? AND note != ''
/// ORDER BY stime DESC LIMIT 1`.
///
/// Returns an empty string when no note exists (REQ §6.5, PHASE-7.6).
pub async fn get_share_note(pool: &DbPool, prefix: &str, fileid: i64) -> String {
    let sql = format!(
        "SELECT note FROM {prefix}share WHERE file_source = $1 AND note != '' \
         ORDER BY stime DESC LIMIT 1"
    );
    sqlx::query_scalar::<_, Option<String>>(&sql)
        .bind(fileid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or_default()
}

/// All `oc_filecache` rows in the subtree of `fc_path` whose `mtime >
/// since_mtime`.
///
/// Pass `since_mtime = -1` to return all rows (initial sync).  The root
/// collection itself is included when its `mtime` satisfies the condition.
///
/// Used by the RFC 6578 `sync-collection` REPORT handler (PHASE-4.11).
pub async fn list_changed_since(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    fc_path: &str,
    since_mtime: i64,
) -> Vec<FileCacheRow> {
    let like_pat = format!("{fc_path}/%");
    let sql = format!(
        "SELECT fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
         size, mtime, storage_mtime, etag, permissions, checksum \
         FROM {prefix}filecache \
         WHERE storage = $1 AND (path = $2 OR path LIKE $3) AND mtime > $4"
    );
    match sqlx::query(&sql)
        .bind(storage_id)
        .bind(fc_path)
        .bind(&like_pat)
        .bind(since_mtime)
        .fetch_all(pool)
        .await
    {
        Err(e) => {
            tracing::error!(error = %e, fc_path = %fc_path, "list_changed_since: SQL error");
            Vec::new()
        }
        Ok(rows) => rows.iter().map(fc_row_from_any).collect(),
    }
}


// ─── private helper ───────────────────────────────────────────────────────────

fn fc_row_from_any(r: &sqlx::any::AnyRow) -> FileCacheRow {
    FileCacheRow {
        fileid: r.get("fileid"),
        storage: r.get("storage"),
        path: r.get("path"),
        path_hash: r.get("path_hash"),
        parent: r.get("parent"),
        name: r.get("name"),
        mimetype: r.get("mimetype"),
        mimepart: r.get("mimepart"),
        size: r.get("size"),
        mtime: r.get("mtime"),
        storage_mtime: r.get("storage_mtime"),
        etag: r.get("etag"),
        permissions: r.get::<Option<i32>, _>("permissions").unwrap_or(0),
        checksum: r.get("checksum"),
        // creation_time and upload_time live in oc_filecache_extended, not
        // oc_filecache.  Default to 0 here; load_meta() calls get_extended()
        // and apply_extended() to fill in the authoritative values.
        creation_time: 0,
        upload_time: 0,
    }
}
