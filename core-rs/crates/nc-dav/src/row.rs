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
/// Match PHP's `\OC\Files\Cache\Storage::adjustStorageId`.
/// Storage IDs longer than 64 characters are stored as their MD5 hex digest
/// in `oc_storages.id` (see `lib/private/Files/Cache/Storage.php:99-105`).
fn adjust_storage_id(id: &str) -> String {
    if id.len() > 64 {
        use md5::Digest;
        format!("{:x}", md5::Md5::digest(id.as_bytes()))
    } else {
        id.to_string()
    }
}

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
        let adjusted = adjust_storage_id(key);
        match sqlx::query(&sql).bind(&adjusted).fetch_optional(pool).await {
            Ok(Some(row)) => return Some(row.get::<i64, _>("numeric_id")),
            Ok(None) => { /* not this key, try next */ }
            Err(e) => {
                tracing::warn!(
                    storage_id = %adjusted,
                    error = %e,
                    "storage lookup query failed"
                );
            }
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
        Ok(Some(_)) => {
            tracing::trace!(path = %path, hash = %hash, storage, "lookup_by_path: found");
        }
        Ok(None) => {
            // Phase 18.1 (round-3 Task 7): the storage-unfiltered fallback
            // query below used to run on EVERY miss — a hidden second query
            // on every new-path PUT/MKCOL existence check.  It exists only
            // to debug hash collisions, so gate it behind trace logging.
            if tracing::enabled!(tracing::Level::TRACE) {
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
                tracing::trace!(
                    path = %path, hash = %hash, storage, ?debug_rows,
                    "lookup_by_path: not found (any storage)"
                );
            }
        }
    }
    match result {
        Err(_) => None,
        Ok(row) => row.map(|r| fc_row_from_any(&r)),
    }
}

/// Look up one filecache row **with its `oc_filecache_extended` metadata** in
/// a single LEFT JOIN (round-3 Task 9).  Files without an extended row get
/// zero times and `metadata_etag = None` — the `get_extended` fallback
/// semantics.  Replaces `lookup_by_path` + `get_extended` (2 queries) in
/// `load_meta` with one.
pub async fn lookup_by_path_with_ext(
    pool: &DbPool,
    prefix: &str,
    storage: i64,
    path: &str,
) -> Option<(FileCacheRow, FileCacheExtRow)> {
    let hash = path_hash(path);
    let sql = format!(
        "SELECT fc.fileid, fc.storage, fc.path, fc.path_hash, fc.parent, fc.name, \
         fc.mimetype, fc.mimepart, fc.size, fc.mtime, fc.storage_mtime, fc.etag, \
         fc.permissions, fc.checksum, fe.metadata_etag, fe.creation_time, fe.upload_time \
         FROM {prefix}filecache fc \
         LEFT JOIN {prefix}filecache_extended fe ON fe.fileid = fc.fileid \
         WHERE fc.storage = $1 AND fc.path_hash = $2"
    );
    match sqlx::query(&sql)
        .bind(storage)
        .bind(&hash)
        .fetch_optional(pool)
        .await
    {
        Ok(Some(r)) => {
            let row = fc_row_from_any(&r);
            let ext = FileCacheExtRow {
                metadata_etag: r.get::<Option<String>, _>("metadata_etag"),
                creation_time: r.get::<Option<i64>, _>("creation_time").unwrap_or(0),
                upload_time: r.get::<Option<i64>, _>("upload_time").unwrap_or(0),
            };
            Some((row, ext))
        }
        Ok(None) => None,
        Err(e) => {
            tracing::error!(error = %e, path = %path, hash = %hash, storage, "lookup_by_path_with_ext: SQL error");
            None
        }
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

/// Fetch all direct children **with their `oc_filecache_extended` metadata**
/// in a single LEFT JOIN — the same shape PHP's `Cache::getFolderContentsById`
/// uses (`selectFileCache` + `selectMetadata`, Cache.php:214).  Children
/// without an extended row get zero times (the `list_extended_batch` fallback
/// semantics); the map is keyed by fileid.
///
/// Round-3 Task 9: replaces `list_children` + `list_extended_batch` (2
/// queries) in `read_dir` with one.
pub async fn list_children_with_ext(
    pool: &DbPool,
    prefix: &str,
    parent_id: i64,
    storage: i64,
) -> (
    Vec<FileCacheRow>,
    std::collections::HashMap<i64, FileCacheExtRow>,
) {
    let sql = format!(
        "SELECT fc.fileid, fc.storage, fc.path, fc.path_hash, fc.parent, fc.name, \
         fc.mimetype, fc.mimepart, fc.size, fc.mtime, fc.storage_mtime, fc.etag, \
         fc.permissions, fc.checksum, fe.metadata_etag, fe.creation_time, fe.upload_time \
         FROM {prefix}filecache fc \
         LEFT JOIN {prefix}filecache_extended fe ON fe.fileid = fc.fileid \
         WHERE fc.parent = $1 AND fc.storage = $2"
    );
    let mut rows_out: Vec<FileCacheRow> = Vec::new();
    let mut ext_map: std::collections::HashMap<i64, FileCacheExtRow> =
        std::collections::HashMap::new();
    match sqlx::query(&sql)
        .bind(parent_id)
        .bind(storage)
        .fetch_all(pool)
        .await
    {
        Err(e) => {
            tracing::error!(error = %e, parent_id = parent_id, "list_children_with_ext: SQL error");
        }
        Ok(rows) => {
            for r in &rows {
                let row = fc_row_from_any(r);
                let fileid = row.fileid;
                let ext = FileCacheExtRow {
                    metadata_etag: r.get::<Option<String>, _>("metadata_etag"),
                    creation_time: r.get::<Option<i64>, _>("creation_time").unwrap_or(0),
                    upload_time: r.get::<Option<i64>, _>("upload_time").unwrap_or(0),
                };
                ext_map.insert(fileid, ext);
                rows_out.push(row);
            }
        }
    }
    (rows_out, ext_map)
}

/// The user's free quota space, mirroring PHP's `Quota::free_space`
/// (Quota.php:74-89): `max(quota - used, 0)` bytes, or `None` when unlimited.
///
/// The quota comes from `oc_preferences` (`files` / `quota`): `none` /
/// `default` → unlimited, a plain number → bytes (human formats such as
/// "1 GB" are not parsed — the differential scenarios use byte values).  The
/// used size is the home storage root's cached size (PHP `getSize(sizeRoot)`).
pub async fn quota_free_space(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    storage_id: i64,
) -> Option<i64> {
    let sql = format!(
        "SELECT configvalue FROM {prefix}preferences \
         WHERE userid = $1 AND appid = 'files' AND configkey = 'quota'"
    );
    let quota = sqlx::query_scalar::<_, String>(&sql)
        .bind(uid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    let quota = match quota.as_deref() {
        None | Some("none") | Some("default") => return None,
        Some(v) => parse_quota_bytes(v)?,
    };
    let used = lookup_by_path(pool, prefix, storage_id, "")
        .await
        .map(|r| r.size)
        .unwrap_or(0);
    Some((quota - used).max(0))
}

/// Parse a quota preference into bytes.  PHP stores quotas in its human
/// format — the OCS provisioning writes `"100 B"`, `"1.5 GB"`, … — plus bare
/// numbers; `"none"`/`"default"` mean unlimited.
fn parse_quota_bytes(v: &str) -> Option<i64> {
    let v = v.trim();
    if let Ok(n) = v.parse::<i64>() {
        return Some(n);
    }
    let lower = v.to_ascii_lowercase();
    let (num, mult) = if let Some(rest) = lower.strip_suffix("tb") {
        (rest, 1i64 << 40)
    } else if let Some(rest) = lower.strip_suffix("gb") {
        (rest, 1i64 << 30)
    } else if let Some(rest) = lower.strip_suffix("mb") {
        (rest, 1i64 << 20)
    } else if let Some(rest) = lower.strip_suffix("kb") {
        (rest, 1i64 << 10)
    } else if let Some(rest) = lower.strip_suffix("b") {
        (rest, 1)
    } else {
        return None;
    };
    let n: f64 = num.trim().parse().ok()?;
    Some((n * mult as f64) as i64)
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

    // Stable statement text per dialect (phase-21 S2): one text bind expanded
    // server-side on Postgres, one bind per id on SQLite.
    let pg = pool.is_postgres();
    let sql = if pg {
        format!(
            "SELECT fileid, metadata_etag, creation_time, upload_time \
             FROM {prefix}filecache_extended \
             WHERE fileid = ANY(string_to_array($1, ',')::bigint[])",
            prefix = prefix,
        )
    } else {
        let placeholders = (1..=fileids.len())
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "SELECT fileid, metadata_etag, creation_time, upload_time \
             FROM {prefix}filecache_extended WHERE fileid IN ({placeholders})",
            prefix = prefix,
        )
    };

    let mut query = sqlx::query(&sql);
    if pg {
        query = query.bind(ids_csv(fileids));
    } else {
        for id in fileids {
            query = query.bind(*id);
        }
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

/// Count direct children of a **batch** of directories in a single query,
/// keyed by parent fileid: `(dir_count, file_count)` per directory.
///
/// Used by `read_dir` so depth-1 PROPFIND computes
/// `{nc:}contained-folder-count` / `{nc:}contained-file-count` for every
/// child directory with one GROUP BY instead of one query per directory.
/// Directories with no children are absent from the map (callers fall back
/// to the single query, which returns `(0, 0)`).
pub async fn count_children_batch(
    pool: &DbPool,
    prefix: &str,
    parent_ids: &[i64],
    storage: i64,
    dir_mimetype_id: i64,
) -> std::collections::HashMap<i64, (i64, i64)> {
    if parent_ids.is_empty() {
        return std::collections::HashMap::new();
    }
    let n = parent_ids.len();
    // Dialect-aware list binding (phase-21 S2): Postgres gets one text bind
    // expanded server-side via string_to_array (stable statement text — no
    // distinct prepared statement per child count); SQLite keeps one bind
    // per id via `IN`.  Same statements, same results on both backends.
    let pg = pool.is_postgres();
    let sql = if pg {
        format!(
            "SELECT parent, \
             count(*) FILTER (WHERE mimetype = $2) AS dirs, \
             count(*) FILTER (WHERE mimetype != $2) AS files \
             FROM {prefix}filecache \
             WHERE parent = ANY(string_to_array($1, ',')::bigint[]) AND storage = $3 \
             GROUP BY parent",
            prefix = prefix,
        )
    } else {
        // $1 is the directory mimetype id (bound first); the IN list starts at $2.
        let placeholders = (2..=n + 1)
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "SELECT parent, \
             count(*) FILTER (WHERE mimetype = $1) AS dirs, \
             count(*) FILTER (WHERE mimetype != $1) AS files \
             FROM {prefix}filecache \
             WHERE parent IN ({placeholders}) AND storage = ${storage} \
             GROUP BY parent",
            prefix = prefix,
            storage = n + 2,
        )
    };
    let mut query = sqlx::query(&sql);
    if pg {
        query = query
            .bind(ids_csv(parent_ids))
            .bind(dir_mimetype_id)
            .bind(storage);
    } else {
        query = query.bind(dir_mimetype_id);
        for id in parent_ids {
            query = query.bind(*id);
        }
        query = query.bind(storage);
    }
    query
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let parent: i64 = r.get("parent");
            let dirs: i64 = r.get::<Option<i64>, _>("dirs").unwrap_or(0);
            let files: i64 = r.get::<Option<i64>, _>("files").unwrap_or(0);
            (parent, (dirs, files))
        })
        .collect()
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

/// Shared `oc_storages` `numeric_id → string_id` cache (phase-21 S3).
///
/// `oc_storages` is tiny and near-static; the table is consulted per node on
/// non-home storages (`get_props`'s `is_mounted` decision).  Negative entries
/// are cached too: storage rows exist before any filecache row referencing
/// them (PHP creates the storage at user creation), so a cached `None` is
/// safe.
pub type SharedStorageCache =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<i64, Option<String>>>>;

/// `get_storage_string_id` with a process-wide cache: hit → return; miss →
/// query + insert (`Some`/`None`).
pub async fn get_storage_string_id_cached(
    pool: &DbPool,
    prefix: &str,
    cache: &SharedStorageCache,
    numeric_id: i64,
) -> Option<String> {
    if let Some(v) = cache.lock().expect("storage cache lock").get(&numeric_id) {
        return v.clone();
    }
    let v = get_storage_string_id(pool, prefix, numeric_id).await;
    cache
        .lock()
        .expect("storage cache lock")
        .insert(numeric_id, v.clone());
    v
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

/// Share details + most-recent share notes for a **batch** of files in one
/// `oc_share` scan (T6.1 merge of the former `share_details_batch` +
/// `share_notes_batch` pair).
///
/// One query fetches every `oc_share` row for the file ids (no WHERE beyond
/// the list — the two consumers filter differently, see below); the rows are
/// split in Rust:
///
/// - **details**: rows passing the `get_share_details` filter (share_type
///   `IN (0,1,3,4,6,7,10,12)` and the user is owner / initiator / share_with),
///   with the same display-name resolution.  Per-file row order preserves
///   the scan order — the pre-merge batch had no `ORDER BY` either, so the
///   emitted `{oc:}share-types` / `{nc:}sharees` XML bytes are unchanged.
/// - **notes**: the most-recent (`stime`-max) row with `note != ''` per
///   file — exactly `get_share_note`'s `WHERE note != '' ORDER BY stime
///   DESC LIMIT 1`.  Note rows are deliberately NOT restricted to the
///   details filter: the most-recent note may live on a share the user is
///   not a party to (the single-row query has no such filter either).
///
/// Files without shares / notes are absent from the respective maps (callers
/// fall back to the single queries, which return `[]` / `""`).
pub async fn share_details_and_notes_batch(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileids: &[i64],
) -> (
    std::collections::HashMap<i64, Vec<ShareDetail>>,
    std::collections::HashMap<i64, String>,
) {
    if fileids.is_empty() {
        return (
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        );
    }
    let pg = pool.is_postgres();
    let sql = if pg {
        format!(
            "SELECT file_source, share_type, share_with, uid_owner, uid_initiator, note, stime \
             FROM {prefix}share \
             WHERE file_source = ANY(string_to_array($1, ',')::bigint[])",
            prefix = prefix,
        )
    } else {
        let placeholders = (1..=fileids.len())
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "SELECT file_source, share_type, share_with, uid_owner, uid_initiator, note, stime \
             FROM {prefix}share \
             WHERE file_source IN ({placeholders})",
            prefix = prefix,
        )
    };
    let mut query = sqlx::query(&sql);
    if pg {
        query = query.bind(ids_csv(fileids));
    } else {
        for id in fileids {
            query = query.bind(*id);
        }
    }
    let rows = match query.fetch_all(pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(uid, error = %e, "share_details_and_notes_batch: SQL error");
            return (
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            );
        }
    };

    // Notes split: most-recent (max stime) non-empty note per file.  An
    // empty-note row with a newer stime must not hide an older note.
    let mut notes: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    let mut best_stime: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for r in &rows {
        let note: String = r.get("note");
        if note.is_empty() {
            continue;
        }
        let file_source: i64 = r.get("file_source");
        let stime: i64 = r.get("stime");
        match best_stime.get(&file_source) {
            Some(prev) if *prev >= stime => continue,
            _ => {
                best_stime.insert(file_source, stime);
                notes.insert(file_source, note);
            }
        }
    }

    // Details split: the `get_share_details` filter, applied in Rust (the
    // scan carries no WHERE beyond the file ids so the notes split sees
    // every row).
    let filtered = rows
        .iter()
        .filter(|r| {
            let share_type: i16 = r.get("share_type");
            if !matches!(share_type, 0 | 1 | 3 | 4 | 6 | 7 | 10 | 12) {
                return false;
            }
            let owner: String = r.get("uid_owner");
            let initiator: Option<String> = r.get("uid_initiator");
            let with: Option<String> = r.get("share_with");
            owner == uid || initiator.as_deref() == Some(uid) || with.as_deref() == Some(uid)
        })
        .collect::<Vec<_>>();

    // Batch-resolve display names for user-type shares (share_type = 0) —
    // one query for every user across all files, same as the single query.
    let user_withs: Vec<String> = filtered
        .iter()
        .filter(|r| r.get::<i16, _>("share_type") == 0)
        .filter_map(|r| r.get::<Option<String>, _>("share_with"))
        .collect();
    let display_names = batch_lookup_display_names(pool, prefix, &user_withs).await;

    let mut out: std::collections::HashMap<i64, Vec<ShareDetail>> =
        std::collections::HashMap::new();
    for r in filtered {
        let file_source: i64 = r.get("file_source");
        let share_type: i16 = r.get("share_type");
        let share_with: Option<String> = r.get("share_with");
        let displayname = match share_type {
            0 => share_with
                .as_ref()
                .and_then(|sw| display_names.get(sw.as_str()).cloned())
                .unwrap_or_else(|| share_with.clone().unwrap_or_default()),
            _ => share_with.clone().unwrap_or_default(),
        };
        out.entry(file_source).or_default().push(ShareDetail {
            share_type,
            share_with,
            share_with_displayname: displayname,
        });
    }
    (out, notes)
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

// ─── oc_properties helpers (task §10.11) ─────────────────────────────────────

/// Parse Clark notation `{namespace}name` → `("namespace", "name")`.
pub fn parse_clark_notation(s: &str) -> Option<(&str, &str)> {
    let inner = s.strip_prefix('{')?;
    let (ns, name) = inner.split_once('}')?;
    Some((ns, name))
}

/// Format a path for `oc_properties.propertypath` (VARCHAR 255).
///
/// Hashes with SHA-1 when the path exceeds 250 bytes, matching PHP's
/// `CustomPropertiesBackend::formatPath()`.
pub fn format_property_path(path: &str) -> String {
    if path.len() > 250 {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(path.as_bytes());
        format!("{:x}", hasher.finalize())
    } else {
        path.to_string()
    }
}

/// List custom properties for a user + path from `oc_properties`.
pub async fn list_custom_properties(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    path: &str,
) -> Vec<(String, String, i16)> {
    let prop_path = format_property_path(path);
    let sql = format!(
        "SELECT propertyname, propertyvalue, valuetype \
         FROM {prefix}properties \
         WHERE userid=$1 AND propertypath=$2"
    );
    let rows = match sqlx::query(&sql)
        .bind(userid)
        .bind(&prop_path)
        .fetch_all(pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "list_custom_properties query failed");
            return vec![];
        }
    };
    rows.iter()
        .map(|r| {
            (
                r.try_get::<String, _>("propertyname").unwrap_or_default(),
                r.try_get::<String, _>("propertyvalue").unwrap_or_default(),
                r.try_get::<i16, _>("valuetype").unwrap_or(1),
            )
        })
        .collect()
}

/// List custom properties for a user and a **batch** of paths in one query.
///
/// Same semantics as `list_custom_properties` (including the >250-char path
/// hash from `format_property_path`); the returned map is keyed by the raw
/// (unhashed) path as passed in.  Paths without properties are absent.
pub async fn custom_properties_batch(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    paths: &[String],
) -> std::collections::HashMap<String, Vec<(String, String, i16)>> {
    if paths.is_empty() {
        return std::collections::HashMap::new();
    }
    // Key the map by the caller's raw path, query by the formatted path.
    let raw_by_formatted: std::collections::HashMap<String, &str> = paths
        .iter()
        .map(|p| (format_property_path(p), p.as_str()))
        .collect();
    // NOTE (phase-21 S2): stays on `IN (...)` — the values are raw fc paths,
    // which may contain commas, so the comma-joined `string_to_array` bind
    // used elsewhere is unsafe here.  Revisit with a real `text[]` bind when
    // the native PgPool lands (plan finding 3/4, Tier 3).
    // $1 is the userid (bound first); the IN list starts at $2.
    let placeholders = (2..=paths.len() + 1)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT propertypath, propertyname, propertyvalue, valuetype \
         FROM {prefix}properties \
         WHERE userid = $1 AND propertypath IN ({placeholders})",
        prefix = prefix,
    );
    let mut query = sqlx::query(&sql).bind(userid);
    for p in paths {
        query = query.bind(format_property_path(p));
    }
    let mut out: std::collections::HashMap<String, Vec<(String, String, i16)>> =
        std::collections::HashMap::new();
    for row in query.fetch_all(pool).await.unwrap_or_default() {
        let prop_path: String = row.get("propertypath");
        if let Some(raw) = raw_by_formatted.get(prop_path.as_str()) {
            out.entry((*raw).to_string()).or_default().push((
                row.get("propertyname"),
                row.get("propertyvalue"),
                row.get("valuetype"),
            ));
        }
    }
    out
}

/// Upsert a custom property — delete-then-insert to avoid PK / composite-key
/// complexity across SQLite and PostgreSQL.
pub async fn upsert_custom_property(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    path: &str,
    propname: &str,
    value_xml: &[u8],
    valuetype: i16,
) -> anyhow::Result<()> {
    let prop_path = format_property_path(path);
    let val_str = std::str::from_utf8(value_xml).unwrap_or("");
    let del_sql = format!(
        "DELETE FROM {prefix}properties \
         WHERE userid=$1 AND propertypath=$2 AND propertyname=$3"
    );
    sqlx::query(&del_sql)
        .bind(userid)
        .bind(&prop_path)
        .bind(propname)
        .execute(pool)
        .await?;
    let ins_sql = format!(
        "INSERT INTO {prefix}properties \
         (userid, propertypath, propertyname, propertyvalue, valuetype) \
         VALUES ($1,$2,$3,$4,$5)"
    );
    sqlx::query(&ins_sql)
        .bind(userid)
        .bind(&prop_path)
        .bind(propname)
        .bind(val_str)
        .bind(valuetype)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete a single custom property by name.
pub async fn delete_custom_property(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    path: &str,
    propname: &str,
) -> anyhow::Result<()> {
    let prop_path = format_property_path(path);
    let sql = format!(
        "DELETE FROM {prefix}properties \
         WHERE userid=$1 AND propertypath=$2 AND propertyname=$3"
    );
    sqlx::query(&sql)
        .bind(userid)
        .bind(&prop_path)
        .bind(propname)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete all custom properties for an exact path (single file/node delete).
pub async fn delete_custom_properties_for_path(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    path: &str,
) -> anyhow::Result<()> {
    let prop_path = format_property_path(path);
    let sql = format!("DELETE FROM {prefix}properties WHERE userid=$1 AND propertypath=$2");
    sqlx::query(&sql)
        .bind(userid)
        .bind(&prop_path)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete custom properties for a directory and all its descendants.
///
/// Queries `oc_filecache` for all paths under the directory, then deletes
/// each one from `oc_properties`.  This avoids LIKE-based queries that would
/// miss hashed (SHA-1) long paths.
pub async fn delete_custom_properties_for_dir(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    storage_id: i64,
    dir_fc_path: &str,
) {
    let like_pat = format!("{dir_fc_path}/%");
    let sql = format!(
        "SELECT path FROM {prefix}filecache \
         WHERE storage=$1 AND (path=$2 OR path LIKE $3)"
    );
    let rows = match sqlx::query(&sql)
        .bind(storage_id)
        .bind(dir_fc_path)
        .bind(&like_pat)
        .fetch_all(pool)
        .await
    {
        Ok(r) => r,
        Err(_) => return,
    };
    for r in &rows {
        let child_path: String = r.try_get("path").unwrap_or_default();
        let _ = delete_custom_properties_for_path(pool, prefix, userid, &child_path).await;
    }
}

/// Update `propertypath` for a single node (rename).
pub async fn update_custom_properties_path(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    old_path: &str,
    new_path: &str,
) -> anyhow::Result<()> {
    let old_prop = format_property_path(old_path);
    let new_prop = format_property_path(new_path);
    let sql = format!(
        "UPDATE {prefix}properties SET propertypath=$1 \
         WHERE userid=$2 AND propertypath=$3"
    );
    sqlx::query(&sql)
        .bind(&new_prop)
        .bind(userid)
        .bind(&old_prop)
        .execute(pool)
        .await?;
    Ok(())
}

/// Update `propertypath` for a directory subtree (rename).
///
/// Queries `oc_filecache` for all descendant paths, then updates each one
/// individually to avoid LIKE-based string prefix replacement that would
/// miss hashed (SHA-1) long paths.
pub async fn update_custom_properties_path_subtree(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    storage_id: i64,
    old_prefix: &str,
    new_prefix: &str,
) {
    let like_pat = format!("{old_prefix}/%");
    let sql = format!(
        "SELECT path FROM {prefix}filecache \
         WHERE storage=$1 AND path LIKE $2"
    );
    let rows = match sqlx::query(&sql)
        .bind(storage_id)
        .bind(&like_pat)
        .fetch_all(pool)
        .await
    {
        Ok(r) => r,
        Err(_) => return,
    };
    for r in &rows {
        let old_child_path: String = r.try_get("path").unwrap_or_default();
        let new_child_path = old_child_path.replacen(old_prefix, new_prefix, 1);
        let _ =
            update_custom_properties_path(pool, prefix, userid, &old_child_path, &new_child_path)
                .await;
    }
}

// ─── Phase 12.3: sharing mask (PHP SetupManager sharing_mask wrapper) ─────────

/// Apply the sharing mask to raw `oc_filecache.permissions`, matching PHP's
/// `PermissionsMask` storage wrapper (`SetupManager.php:176-189`).
///
/// When sharing is disabled for the user, the SHARE bit (16) is stripped.
/// `PERMISSION_ALL - PERMISSION_SHARE = 31 - 16 = 15`.
pub fn apply_sharing_mask(raw_permissions: i32, sharing_disabled: bool) -> i32 {
    if sharing_disabled {
        raw_permissions & 15 // PERMISSION_ALL - PERMISSION_SHARE
    } else {
        raw_permissions
    }
}

// ─── Phase 12.4: share-permissions (PHP Node::getSharePermissions) ────────────

/// Compute the `{ocs:}share-permissions` value matching PHP
/// `Node::getSharePermissions()` (`apps/dav/lib/Connector/Sabre/Node.php:235-276`).
///
/// For non-shared storage (the only kind Rust supports today): returns the node's
/// own `oc_filecache.permissions`, with DELETE|UPDATE OR-ed in for a non-moveable,
/// non-readonly mount root, and CREATE|DELETE cleared for files.
///
/// Constants (from `\OCP\Constants`): READ=1, UPDATE=2, CREATE=4, DELETE=8, SHARE=16.
pub fn compute_share_permissions(raw_permissions: i32, is_dir: bool, is_mount_root: bool) -> i32 {
    let mut perms = raw_permissions;

    // PHP lines 261-275: mount roots of non-moveable, non-readonly mounts
    // always gain DELETE|UPDATE.  Home storage's "files" root satisfies this.
    if is_mount_root {
        perms |= 8 | 2; // PERMISSION_DELETE | PERMISSION_UPDATE
    }

    // PHP lines 280-282: files can't have CREATE or DELETE
    if !is_dir {
        perms &= !(4 | 8); // clear PERMISSION_CREATE | PERMISSION_DELETE
    }

    perms
}

/// Map an NC permission bitmask to OCM share-permissions JSON array string,
/// matching PHP `FilesPlugin::ncPermissions2ocmPermissions()`.
///
/// SHARE(16) → "share", READ(1) → "read", CREATE(4)|UPDATE(2) → "write".
pub fn permissions_to_ocm_json(permissions: i32) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if permissions & 16 != 0 {
        parts.push("\"share\"");
    }
    if permissions & 1 != 0 {
        parts.push("\"read\"");
    }
    if permissions & 4 != 0 || permissions & 2 != 0 {
        parts.push("\"write\"");
    }
    format!("[{}]", parts.join(","))
}

// ─── Phase 12.5: share-types / sharees ─────────────────────────────────────────

/// One share row for the `{oc:}share-types` and `{nc:}sharees` properties.
#[derive(Debug, Clone)]
pub struct ShareDetail {
    pub share_type: i16,
    pub share_with: Option<String>,
    /// Resolved display name; falls back to `share_with` for non-user types.
    pub share_with_displayname: String,
}

/// Return share details for a file node, matching PHP `SharesPlugin::getShare()`.
///
/// Queries shares **by** the user (`uid_owner` / `uid_initiator`) plus shares
/// **with** the user (`share_with`), for all PHP share types (USER, GROUP, LINK,
/// EMAIL, REMOTE, CIRCLE, ROOM, DECK).
pub async fn get_share_details(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileid: i64,
) -> Vec<ShareDetail> {
    let sql = format!(
        "SELECT DISTINCT share_type, share_with \
         FROM {prefix}share \
         WHERE file_source = $1 \
         AND share_type IN (0,1,3,4,6,7,10,12) \
         AND (uid_owner = $2 OR uid_initiator = $3 OR share_with = $4)",
        prefix = prefix
    );
    let rows = match sqlx::query(&sql)
        .bind(fileid)
        .bind(uid)
        .bind(uid)
        .bind(uid)
        .fetch_all(pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(fileid, uid, error = %e, "get_share_details: SQL error");
            return vec![];
        }
    };

    // Batch-resolve display names for user-type shares (share_type = 0).
    let user_withs: Vec<String> = rows
        .iter()
        .filter(|r| r.get::<i16, _>("share_type") == 0)
        .filter_map(|r| r.get::<Option<String>, _>("share_with"))
        .collect();
    let display_names = if !user_withs.is_empty() {
        batch_lookup_display_names(pool, prefix, &user_withs).await
    } else {
        std::collections::HashMap::new()
    };

    rows.iter()
        .map(|r| {
            let share_type: i16 = r.get("share_type");
            let share_with: Option<String> = r.get("share_with");
            let displayname = match share_type {
                0 => share_with
                    .as_ref()
                    .and_then(|sw| display_names.get(sw.as_str()).cloned())
                    .unwrap_or_else(|| share_with.clone().unwrap_or_default()),
                _ => share_with.clone().unwrap_or_default(),
            };
            ShareDetail {
                share_type,
                share_with,
                share_with_displayname: displayname,
            }
        })
        .collect()
}

async fn batch_lookup_display_names(
    pool: &DbPool,
    prefix: &str,
    uids: &[String],
) -> std::collections::HashMap<String, String> {
    if uids.is_empty() {
        return std::collections::HashMap::new();
    }

    let mut display_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut unresolved: Vec<&String> = Vec::new();

    // 1. Batch-query oc_users.displayname first — PHP's primary source
    //    (User::getDisplayName → backend); see lookup_user_display_name for
    //    why oc_accounts is only a (potentially stale) fallback.
    //    Uids are comma-safe (Nextcloud usernames: letters/digits/`_.@-'`).
    let pg = pool.is_postgres();
    let users_sql = if pg {
        format!(
            "SELECT uid, displayname FROM {prefix}users \
             WHERE uid = ANY(string_to_array($1, ',')::text[])",
            prefix = prefix
        )
    } else {
        let placeholders = uids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("${}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "SELECT uid, displayname FROM {prefix}users WHERE uid IN ({placeholders})",
            prefix = prefix
        )
    };
    let mut query = sqlx::query(&users_sql);
    if pg {
        query = query.bind(uids.join(","));
    } else {
        for uid in uids {
            query = query.bind(uid);
        }
    }
    let user_rows = query.fetch_all(pool).await.unwrap_or_default();
    for row in user_rows {
        let uid: String = row.get("uid");
        let dn: Option<String> = row.get("displayname");
        if let Some(dn) = dn.filter(|s| !s.is_empty()) {
            display_names.insert(uid, dn);
        }
    }

    // Collect UIDs with no oc_users displayname for the oc_accounts fallback.
    for uid in uids {
        if !display_names.contains_key(uid.as_str()) {
            unresolved.push(uid);
        }
    }

    // 2. Batch-query oc_accounts for the remaining UIDs (display names in
    //    JSON under data->'displayname'->>'value').
    if !unresolved.is_empty() {
        let accounts_sql = if pg {
            format!(
                "SELECT uid, data FROM {prefix}accounts \
                 WHERE uid = ANY(string_to_array($1, ',')::text[])",
                prefix = prefix
            )
        } else {
            let users_placeholders = unresolved
                .iter()
                .enumerate()
                .map(|(i, _)| format!("${}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "SELECT uid, data FROM {prefix}accounts WHERE uid IN ({users_placeholders})",
                prefix = prefix
            )
        };
        let mut query = sqlx::query(&accounts_sql);
        if pg {
            let csv = unresolved
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",");
            query = query.bind(csv);
        } else {
            for uid in &unresolved {
                query = query.bind(uid);
            }
        }
        let account_rows: Vec<(String, String)> = query
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                let uid: String = r.get("uid");
                let data: String = r.get("data");
                (uid, data)
            })
            .collect();
        for (uid, data) in &account_rows {
            if let Some(dn) = extract_displayname_from_accounts_json(data) {
                display_names.entry(uid.clone()).or_insert(dn);
            }
        }
        // 3. For any UIDs still unresolved, fall back to the UID itself.
        for uid in &unresolved {
            display_names
                .entry((*uid).clone())
                .or_insert_with(|| (*uid).clone());
        }
    }

    // 4. For UIDs that had no oc_users row and no oc_accounts row, fall back to UID.
    for uid in uids {
        display_names
            .entry(uid.clone())
            .or_insert_with(|| uid.clone());
    }

    display_names
}

/// Extract the display name from an `oc_accounts.data` JSON value.
///
/// PHP stores: `{"displayname":{"value":"Tan Siew Kin","scope":"...","verified":"0"},...}`
fn extract_displayname_from_accounts_json(data: &str) -> Option<String> {
    // Use simple JSON parsing via serde_json.
    let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
    parsed
        .get("displayname")?
        .get("value")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Format share types as XML matching PHP `ShareTypeList::xmlSerialize()`.
///
/// Empty when no shares → `<oc:share-types/>` (self-closing, handled by the
/// fact we emit content as raw inner XML).
pub fn format_share_types_xml(types: &[i32]) -> String {
    if types.is_empty() {
        return String::new();
    }
    let mut xml = String::new();
    for t in types {
        xml.push_str(&format!(
            "<oc:share-type xmlns:oc=\"http://owncloud.org/ns\">{t}</oc:share-type>"
        ));
    }
    xml
}

/// Format sharees as XML matching PHP `ShareeList::xmlSerialize()`.
pub fn format_sharees_xml(details: &[ShareDetail]) -> String {
    if details.is_empty() {
        return String::new();
    }
    let mut xml = String::new();
    for d in details {
        xml.push_str(&format!(
            "<nc:sharee xmlns:nc=\"http://nextcloud.org/ns\">\
             <nc:id>{}</nc:id>\
             <nc:display-name>{}</nc:display-name>\
             <nc:type>{}</nc:type>\
             </nc:sharee>",
            d.share_with.as_deref().unwrap_or(""),
            d.share_with_displayname,
            d.share_type,
        ));
    }
    xml
}

// ─── Phase 12.6: comments properties ───────────────────────────────────────────

/// Return the number of top-level comments for a file, matching PHP
/// `ICommentsManager::getNumberOfCommentsForObject('files', $id)`.
pub async fn get_comments_count(pool: &DbPool, prefix: &str, fileid: i64) -> i64 {
    let sql = format!(
        "SELECT COUNT(*) FROM {prefix}comments \
         WHERE object_type = 'files' AND object_id = $1",
        prefix = prefix
    );
    sqlx::query_scalar::<_, Option<i64>>(&sql)
        .bind(fileid.to_string())
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or(0)
}

/// Return the number of unread comments for a file and user, matching PHP
/// `ICommentsManager::getNumberOfUnreadCommentsForObjects()`.
///
/// The read marker is a de-correlated `LEFT JOIN` (T6.4), same shape as the
/// batch query and PHP's own `Manager.php:678-688`.
pub async fn get_comments_unread(pool: &DbPool, prefix: &str, fileid: i64, uid: &str) -> i64 {
    let sql = format!(
        "SELECT COUNT(*) FROM {prefix}comments c \
         LEFT JOIN {prefix}comments_read_markers m \
           ON m.user_id = $2 AND m.object_type = 'files' AND m.object_id = c.object_id \
         WHERE c.object_type = 'files' AND c.object_id = $1 \
         AND c.actor_type = 'users' AND c.actor_id != $2 \
         AND c.creation_timestamp > COALESCE(m.marker_datetime, '1970-01-01 00:00:00')",
        prefix = prefix
    );
    sqlx::query_scalar::<_, Option<i64>>(&sql)
        .bind(fileid.to_string())
        .bind(uid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or(0)
}

/// Comment counts + unread counts for a **batch** of files in one query,
/// keyed by fileid: `(count, unread)`.
///
/// T6.3 merge of the former `comments_counts_batch` + `comments_unread_batch`
/// pair — one `GROUP BY c.object_id` with `COUNT(*)` and the unread
/// predicate as `count(*) FILTER (WHERE …)`.  T6.4 de-correlates the read
/// marker: a `LEFT JOIN` (PK `(user_id, object_type, object_id)` on the live
/// schema — at most one marker row per comment row, so `COUNT(*)` is
/// unaffected) with `COALESCE(m.marker_datetime, epoch)` — the same shape
/// PHP's `CommentsManager::getNumberOfUnreadCommentsForObjects` uses
/// (`Manager.php:678-688`).  Mirrors `get_comments_count` +
/// `get_comments_unread`; files without comments are absent from the map
/// (callers fall back to the single queries, which return 0).
pub async fn comments_counts_batch(
    pool: &DbPool,
    prefix: &str,
    fileids: &[i64],
    uid: &str,
) -> std::collections::HashMap<i64, (i64, i64)> {
    if fileids.is_empty() {
        return std::collections::HashMap::new();
    }
    let n = fileids.len();
    let pg = pool.is_postgres();
    let sql = if pg {
        format!(
            "SELECT c.object_id, COUNT(*) AS n, \
             count(*) FILTER (WHERE c.actor_type = 'users' AND c.actor_id != $2 \
                 AND c.creation_timestamp > COALESCE(m.marker_datetime, '1970-01-01 00:00:00')) AS unread \
             FROM {prefix}comments c \
             LEFT JOIN {prefix}comments_read_markers m \
               ON m.user_id = $2 AND m.object_type = 'files' AND m.object_id = c.object_id \
             WHERE c.object_type = 'files' \
             AND c.object_id = ANY(string_to_array($1, ',')::text[]) \
             GROUP BY c.object_id",
            prefix = prefix,
        )
    } else {
        let placeholders = (1..=n)
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "SELECT c.object_id, COUNT(*) AS n, \
             count(*) FILTER (WHERE c.actor_type = 'users' AND c.actor_id != ${uid} \
                 AND c.creation_timestamp > COALESCE(m.marker_datetime, '1970-01-01 00:00:00')) AS unread \
             FROM {prefix}comments c \
             LEFT JOIN {prefix}comments_read_markers m \
               ON m.user_id = ${uid} AND m.object_type = 'files' AND m.object_id = c.object_id \
             WHERE c.object_type = 'files' AND c.object_id IN ({placeholders}) \
             GROUP BY c.object_id",
            prefix = prefix,
            uid = n + 1,
        )
    };
    let mut query = sqlx::query(&sql);
    if pg {
        query = query.bind(ids_csv(fileids));
    } else {
        for id in fileids {
            query = query.bind(id.to_string());
        }
    }
    query = query.bind(uid);
    query
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let object_id: String = r.get("object_id");
            let n: i64 = r.get("n");
            let unread: i64 = r.get::<Option<i64>, _>("unread").unwrap_or(0);
            (object_id.parse::<i64>().unwrap_or(0), (n, unread))
        })
        .collect()
}

/// Build the `{oc:}comments-href` URL, matching PHP
/// `CommentPropertiesPlugin::getCommentsLink()`.
///
/// The format is: `{base_url}/remote.php/dav/comments/files/{fileid}`
pub fn build_comments_href(base_url: &str, fileid: i64) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{base}/remote.php/dav/comments/files/{fileid}")
}

/// Batch query for comments counts across multiple file IDs.
///
/// Returns a `HashMap<fileid, count>`.  Files with no comments are absent.
pub async fn batch_comments_counts(
    pool: &DbPool,
    prefix: &str,
    fileids: &[i64],
) -> std::collections::HashMap<i64, i64> {
    if fileids.is_empty() {
        return std::collections::HashMap::new();
    }
    let placeholders = fileids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT object_id, COUNT(*) AS cnt FROM {prefix}comments \
         WHERE object_type = 'files' AND object_id IN ({placeholders}) \
         GROUP BY object_id",
        prefix = prefix
    );
    let mut query = sqlx::query(&sql);
    for id in fileids {
        query = query.bind(id.to_string());
    }
    query
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let object_id: String = r.get("object_id");
            let cnt: i64 = r.get("cnt");
            object_id.parse::<i64>().ok().map(|fid| (fid, cnt))
        })
        .collect()
}

/// Batch query for unread comments counts across multiple file IDs for a user.
pub async fn batch_comments_unread(
    pool: &DbPool,
    prefix: &str,
    fileids: &[i64],
    uid: &str,
) -> std::collections::HashMap<i64, i64> {
    if fileids.is_empty() {
        return std::collections::HashMap::new();
    }
    // For each file, count comments whose creation_timestamp exceeds the user's
    // read marker.  We process per-file since the marker query is per-file.
    let mut map = std::collections::HashMap::new();
    for &fid in fileids {
        let fid_str = fid.to_string();
        let sql = format!(
            "SELECT COUNT(*) FROM {prefix}comments \
             WHERE object_type = 'files' AND object_id = $1 \
             AND actor_type = 'users' AND actor_id != $2 \
             AND creation_timestamp > COALESCE( \
                 (SELECT marker_datetime FROM {prefix}comments_read_markers \
                  WHERE user_id = $3 AND object_type = 'files' AND object_id = $4), \
                 TIMESTAMP '1970-01-01 00:00:00' \
             )",
            prefix = prefix
        );
        if let Ok(Some(cnt)) = sqlx::query_scalar::<_, Option<i64>>(&sql)
            .bind(&fid_str)
            .bind(uid)
            .bind(uid)
            .bind(&fid_str)
            .fetch_optional(pool)
            .await
        {
            if let Some(c) = cnt {
                if c > 0 {
                    map.insert(fid, c);
                }
            }
        }
    }
    map
}

// ─── Phase 12.7: system tags ───────────────────────────────────────────────────

/// One system tag row from `oc_systemtag` joined with `oc_systemtag_object_mapping`.
#[derive(Debug, Clone)]
pub struct SystemTagRow {
    pub id: i64,
    pub name: String,
    pub user_visible: bool,
    pub user_assignable: bool,
    pub color: Option<String>,
}

/// Return system tags for a file, matching PHP `SystemTagPlugin::getTagsForFile()`.
///
/// Tags are filtered for user visibility and sorted by natural-sort name
/// (we approximate with case-insensitive alphanumeric order).
pub async fn get_system_tags_for_file(
    pool: &DbPool,
    prefix: &str,
    fileid: i64,
) -> Vec<SystemTagRow> {
    let sql = format!(
        "SELECT t.id, t.name, t.visibility, t.editable, t.color \
         FROM {prefix}systemtag t \
         JOIN {prefix}systemtag_object_mapping m \
           ON m.systemtagid = t.id \
         WHERE m.objectid = $1 AND m.objecttype = 'files' \
         AND t.visibility = 1 \
         ORDER BY LOWER(t.name)",
        prefix = prefix
    );
    match sqlx::query(&sql)
        .bind(fileid.to_string())
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows
            .iter()
            .map(|r| SystemTagRow {
                id: r.get("id"),
                name: r.get("name"),
                user_visible: r.get::<i16, _>("visibility") == 1,
                user_assignable: r.get::<i16, _>("editable") == 1,
                color: r.get("color"),
            })
            .collect(),
        Err(e) => {
            tracing::error!(fileid, error = %e, "get_system_tags_for_file: SQL error");
            vec![]
        }
    }
}

/// System tags for a **batch** of files in one query, keyed by fileid.
///
/// Mirrors `get_system_tags_for_file` (user-visible only, sorted by
/// case-insensitive name); the per-file order matches the single query.
/// Files without tags are absent from the map.
pub async fn system_tags_batch(
    pool: &DbPool,
    prefix: &str,
    fileids: &[i64],
) -> std::collections::HashMap<i64, Vec<SystemTagRow>> {
    if fileids.is_empty() {
        return std::collections::HashMap::new();
    }
    let pg = pool.is_postgres();
    let sql = if pg {
        format!(
            "SELECT m.objectid, t.id, t.name, t.visibility, t.editable, t.color \
             FROM {prefix}systemtag t \
             JOIN {prefix}systemtag_object_mapping m ON m.systemtagid = t.id \
             WHERE m.objectid = ANY(string_to_array($1, ',')::text[]) AND m.objecttype = 'files' \
             AND t.visibility = 1 \
             ORDER BY LOWER(t.name)",
            prefix = prefix,
        )
    } else {
        let placeholders = (1..=fileids.len())
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "SELECT m.objectid, t.id, t.name, t.visibility, t.editable, t.color \
             FROM {prefix}systemtag t \
             JOIN {prefix}systemtag_object_mapping m ON m.systemtagid = t.id \
             WHERE m.objectid IN ({placeholders}) AND m.objecttype = 'files' \
             AND t.visibility = 1 \
             ORDER BY LOWER(t.name)",
            prefix = prefix,
        )
    };
    let mut query = sqlx::query(&sql);
    if pg {
        query = query.bind(ids_csv(fileids));
    } else {
        for id in fileids {
            query = query.bind(id.to_string());
        }
    }
    let mut out: std::collections::HashMap<i64, Vec<SystemTagRow>> =
        std::collections::HashMap::new();
    for row in query.fetch_all(pool).await.unwrap_or_default() {
        let object_id: String = row.get("objectid");
        let fileid = object_id.parse::<i64>().unwrap_or(0);
        out.entry(fileid).or_default().push(SystemTagRow {
            id: row.get("id"),
            name: row.get("name"),
            user_visible: row.get::<i16, _>("visibility") == 1,
            user_assignable: row.get::<i16, _>("editable") == 1,
            color: row.get("color"),
        });
    }
    out
}

/// Format system tags as XML matching PHP `SystemTagList::xmlSerialize()`.
///
/// PHP wraps in `<nc:system-tags>` with child `<nc:system-tag>` elements.
/// Each tag element contains `{oc:}id`, `{nc:}display-name`, `{nc:}user-visible`,
/// `{nc:}user-assignable`, `{nc:}can-assign`, and optionally `{nc:}color`.
pub fn format_system_tags_xml(tags: &[SystemTagRow], _can_assign_all: bool) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut xml = String::new();
    for t in tags {
        let color_attr = t
            .color
            .as_ref()
            .filter(|c| !c.is_empty())
            .map(|c| format!("<nc:color>{c}</nc:color>"))
            .unwrap_or_default();
        xml.push_str(&format!(
            "<nc:system-tag xmlns:nc=\"http://nextcloud.org/ns\">\
             <oc:id xmlns:oc=\"http://owncloud.org/ns\">{id}</oc:id>\
             <nc:display-name>{name}</nc:display-name>\
             <nc:user-visible>{uv}</nc:user-visible>\
             <nc:user-assignable>{ua}</nc:user-assignable>\
             <nc:can-assign>{ua}</nc:can-assign>\
             {color}\
             </nc:system-tag>",
            id = t.id,
            name = t.name,
            uv = if t.user_visible { "true" } else { "false" },
            ua = if t.user_assignable { "true" } else { "false" },
            color = color_attr,
        ));
    }
    xml
}

// ─── Phase 9.8: filter-files REPORT helpers ─────────────────────────────────────

/// Return all file IDs favorited by a user, matching PHP
/// `$fileTagger->load('files')->getFavorites()`.
///
/// Queries `oc_vcategory` (for the favorite sentinel) joined with
/// `oc_vcategory_to_object` to get the `objid` (filecache fileid).
pub async fn get_favorite_fileids(pool: &DbPool, prefix: &str, uid: &str) -> Vec<i64> {
    let sql = format!(
        "SELECT vco.objid FROM {prefix}vcategory_to_object vco \
         JOIN {prefix}vcategory vc ON vc.id = vco.categoryid \
         WHERE vc.uid = $1 AND vc.type = 'files' AND vc.category = $2",
        prefix = prefix
    );
    match sqlx::query_scalar::<_, String>(&sql)
        .bind(uid)
        .bind(crate::tags::TAG_FAVORITE)
        .fetch_all(pool)
        .await
    {
        Ok(ids) => ids
            .into_iter()
            .filter_map(|s: String| s.parse::<i64>().ok())
            .collect(),
        Err(e) => {
            tracing::error!(uid, error = %e, "get_favorite_fileids: SQL error");
            vec![]
        }
    }
}

/// Batch-lookup `oc_filecache` rows by file IDs.
///
/// Returns a `HashMap<fileid, FileCacheRow>`.  Files not found are absent.
/// Used by the `filter-files` REPORT to look up matching nodes.
pub async fn lookup_by_ids(
    pool: &DbPool,
    prefix: &str,
    fileids: &[i64],
) -> std::collections::HashMap<i64, FileCacheRow> {
    if fileids.is_empty() {
        return std::collections::HashMap::new();
    }
    let pg = pool.is_postgres();
    let sql = if pg {
        format!(
            "SELECT fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
             size, mtime, storage_mtime, etag, permissions, checksum, \
             creation_time, upload_time \
             FROM {prefix}filecache WHERE fileid = ANY(string_to_array($1, ',')::bigint[])",
            prefix = prefix
        )
    } else {
        let placeholders = fileids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("${}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "SELECT fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
             size, mtime, storage_mtime, etag, permissions, checksum, \
             creation_time, upload_time \
             FROM {prefix}filecache WHERE fileid IN ({placeholders})",
            prefix = prefix
        )
    };
    let mut query = sqlx::query(&sql);
    if pg {
        query = query.bind(ids_csv(fileids));
    } else {
        for id in fileids {
            query = query.bind(*id);
        }
    }
    query
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let row = fc_row_from_any(&r);
            (row.fileid, row)
        })
        .collect()
}

// ─── private helper ───────────────────────────────────────────────────────────

/// i64 ids as one comma-joined string for the Postgres `string_to_array`
/// bind.  The Any driver cannot bind arrays (sqlx-core any/value.rs has no
/// Array kind), so batch lists become one text bind on Postgres vs one bind
/// per id on SQLite (phase-21 S2).  Statement text is then stable — no
/// distinct prepared statement per list size.
fn ids_csv(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_clark_notation ──────────────────────────────────────────────

    #[test]
    fn parse_clark_notation_basic() {
        assert_eq!(
            parse_clark_notation("{urn:example}state"),
            Some(("urn:example", "state"))
        );
    }

    #[test]
    fn parse_clark_notation_dav_namespace() {
        assert_eq!(
            parse_clark_notation("{DAV:}getetag"),
            Some(("DAV:", "getetag"))
        );
    }

    #[test]
    fn parse_clark_notation_nc_namespace() {
        assert_eq!(
            parse_clark_notation("{http://nextcloud.org/ns}creation_time"),
            Some(("http://nextcloud.org/ns", "creation_time"))
        );
    }

    #[test]
    fn parse_clark_notation_no_brace_returns_none() {
        assert_eq!(parse_clark_notation("no-brace"), None);
    }

    #[test]
    fn parse_clark_notation_no_closing_brace_returns_none() {
        assert_eq!(parse_clark_notation("{nsname"), None);
    }

    #[test]
    fn parse_clark_notation_empty_returns_none() {
        assert_eq!(parse_clark_notation(""), None);
    }

    // ── format_property_path ──────────────────────────────────────────────

    #[test]
    fn format_property_path_short_path_is_unchanged() {
        let path = "files/Documents/note.txt";
        assert_eq!(format_property_path(path), path);
    }

    #[test]
    fn format_property_path_exactly_250_chars_is_unchanged() {
        let path = "f".repeat(250);
        assert_eq!(format_property_path(&path), path);
    }

    #[test]
    fn format_property_path_251_chars_is_hashed() {
        let path = "f".repeat(251);
        let result = format_property_path(&path);
        // SHA-1 hex digest is 40 chars
        assert_eq!(result.len(), 40);
        assert_ne!(result, path);
    }

    #[test]
    fn format_property_path_very_long_path_is_hashed() {
        let path = "files/".to_string() + &"x".repeat(500);
        let result = format_property_path(&path);
        assert_eq!(result.len(), 40);
    }

    #[test]
    fn format_property_path_consistent_hash() {
        let path = "a".repeat(300);
        let a = format_property_path(&path);
        let b = format_property_path(&path);
        assert_eq!(a, b);
    }

    // ── Batch-vs-single consistency (Phase 18.1) ──────────────────────────
    //
    // The `*_batch` queries must return exactly what the per-node queries
    // return; the difftest suite is the real gate, these pin the mapping.

    /// In-memory SQLite with the tables the batch PROPFIND queries read.
    async fn fresh_batch_db() -> DbPool {
        let pool = DbPool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("in-memory SQLite"),
        );
        sqlx::query(
            "CREATE TABLE oc_filecache (
                fileid BIGINT NOT NULL PRIMARY KEY, storage BIGINT NOT NULL,
                path VARCHAR(4000) NOT NULL DEFAULT '', path_hash VARCHAR(32) NOT NULL DEFAULT '',
                parent BIGINT NOT NULL DEFAULT 0, name VARCHAR(250),
                mimetype BIGINT NOT NULL DEFAULT 0, mimepart BIGINT NOT NULL DEFAULT 0,
                size BIGINT NOT NULL DEFAULT 0, mtime BIGINT NOT NULL DEFAULT 0,
                storage_mtime BIGINT NOT NULL DEFAULT 0, etag VARCHAR(40),
                permissions INTEGER NOT NULL DEFAULT 0, checksum VARCHAR(255)
            )",
        )
        .execute(&pool)
        .await
        .expect("filecache");
        sqlx::query(
            "CREATE TABLE oc_share (
                id BIGINT NOT NULL PRIMARY KEY, share_type SMALLINT NOT NULL DEFAULT 0,
                share_with VARCHAR(255), uid_owner VARCHAR(64) NOT NULL DEFAULT '',
                uid_initiator VARCHAR(64), file_source BIGINT, stime BIGINT NOT NULL DEFAULT 0,
                note TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .expect("share");
        sqlx::query(
            "CREATE TABLE oc_comments (
                id BIGINT NOT NULL PRIMARY KEY, object_type VARCHAR(64) NOT NULL DEFAULT '',
                object_id VARCHAR(64) NOT NULL DEFAULT '', actor_type VARCHAR(64) NOT NULL DEFAULT '',
                actor_id VARCHAR(64) NOT NULL DEFAULT '', creation_timestamp TIMESTAMP
            )",
        )
        .execute(&pool)
        .await
        .expect("comments");
        sqlx::query(
            // PK mirrors the live PostgreSQL schema (verified 2026-08-13) —
            // the de-correlated LEFT JOIN (T6.4) relies on it for at-most-one
            // marker row per (user, object).
            "CREATE TABLE oc_comments_read_markers (
                user_id VARCHAR(64) NOT NULL, object_type VARCHAR(64) NOT NULL DEFAULT '',
                object_id VARCHAR(64) NOT NULL DEFAULT '', marker_datetime TIMESTAMP,
                PRIMARY KEY (user_id, object_type, object_id)
            )",
        )
        .execute(&pool)
        .await
        .expect("markers");
        sqlx::query(
            "CREATE TABLE oc_systemtag (
                id BIGINT NOT NULL PRIMARY KEY, name VARCHAR(255) NOT NULL DEFAULT '',
                visibility SMALLINT NOT NULL DEFAULT 1, editable SMALLINT NOT NULL DEFAULT 1,
                color VARCHAR(255)
            )",
        )
        .execute(&pool)
        .await
        .expect("systemtag");
        sqlx::query(
            "CREATE TABLE oc_systemtag_object_mapping (
                objectid VARCHAR(64) NOT NULL DEFAULT '', objecttype VARCHAR(64) NOT NULL DEFAULT '',
                systemtagid BIGINT NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("mapping");
        sqlx::query(
            "CREATE TABLE oc_users (uid VARCHAR(64) NOT NULL PRIMARY KEY, displayname VARCHAR(64))",
        )
        .execute(&pool)
        .await
        .expect("users");
        sqlx::query(
            "CREATE TABLE oc_properties (
                id INTEGER NOT NULL PRIMARY KEY, userid VARCHAR(64) NOT NULL DEFAULT '',
                propertypath VARCHAR(255) NOT NULL DEFAULT '', propertyname VARCHAR(255) NOT NULL DEFAULT '',
                propertyvalue TEXT NOT NULL DEFAULT '', valuetype SMALLINT NOT NULL DEFAULT 1
            )",
        )
        .execute(&pool)
        .await
        .expect("properties");
        sqlx::query(
            "CREATE TABLE oc_filecache_extended (
                fileid BIGINT NOT NULL PRIMARY KEY, metadata_etag VARCHAR(40),
                creation_time INTEGER NOT NULL DEFAULT 0, upload_time INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("filecache_extended");
        pool
    }

    #[tokio::test]
    async fn with_ext_variants_match_singles() {
        let pool = fresh_batch_db().await;
        let prefix = "oc_";
        // dir (1) with two children: a.txt (has an extended row), b.txt (none).
        for (id, parent, name, mime) in [(1, 0, "files", 2), (2, 1, "a.txt", 1), (3, 1, "b.txt", 1)]
        {
            let path = format!("files/{name}");
            sqlx::query(
                "INSERT INTO oc_filecache (fileid, storage, path, path_hash, parent, name, mimetype) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(1)
            .bind(&path)
            .bind(path_hash(&path))
            .bind(parent)
            .bind(name)
            .bind(mime)
            .execute(&pool)
            .await
            .expect("insert");
        }
        sqlx::query(
            "INSERT INTO oc_filecache_extended (fileid, metadata_etag, creation_time, upload_time) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(2)
        .bind("etag-42")
        .bind(42)
        .bind(43)
        .execute(&pool)
        .await
        .expect("ext insert");

        // lookup variant: extended row present → real values; absent → zeros.
        let (row, ext) = lookup_by_path_with_ext(&pool, prefix, 1, "files/a.txt")
            .await
            .expect("a.txt");
        assert_eq!(row.fileid, 2);
        assert_eq!(ext.creation_time, 42);
        assert_eq!(ext.upload_time, 43);
        assert_eq!(ext.metadata_etag.as_deref(), Some("etag-42"));
        let (row, ext) = lookup_by_path_with_ext(&pool, prefix, 1, "files/b.txt")
            .await
            .expect("b.txt");
        assert_eq!(row.fileid, 3);
        assert_eq!(ext.creation_time, 0, "absent extended row → zero times");
        assert_eq!(ext.upload_time, 0);
        assert_eq!(ext.metadata_etag, None);
        assert!(
            lookup_by_path_with_ext(&pool, prefix, 1, "files/missing.txt")
                .await
                .is_none()
        );

        // list variant: same values through the fileid-keyed map, consistent
        // with the single-query pair for every child.
        let (rows, map) = list_children_with_ext(&pool, prefix, 1, 1).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(map.get(&2).unwrap().creation_time, 42);
        assert_eq!(map.get(&3).unwrap().creation_time, 0);
        for r in &rows {
            let single_row = lookup_by_path(&pool, prefix, 1, r.path.as_deref().unwrap())
                .await
                .expect("single row");
            let single_ext = get_extended(&pool, prefix, r.fileid).await;
            let joined_ext = map.get(&r.fileid).expect("joined ext");
            assert_eq!(
                joined_ext.creation_time, single_ext.creation_time,
                "fileid {}",
                r.fileid
            );
            assert_eq!(joined_ext.upload_time, single_ext.upload_time);
            assert_eq!(joined_ext.metadata_etag, single_ext.metadata_etag);
            assert_eq!(single_row.fileid, r.fileid);
        }
    }

    #[tokio::test]
    async fn count_children_batch_matches_single() {
        let pool = fresh_batch_db().await;
        let prefix = "oc_";
        // mimetype 2 = directory, 1 = file, all on storage 1.
        for (id, parent, name, mime) in [
            (1, 0, "files", 2),
            (2, 1, "a", 2),     // dir with one subdir + one file
            (3, 1, "b", 2),     // empty dir
            (4, 1, "c.txt", 1), // file
            (5, 2, "x.txt", 1),
            (6, 2, "sub", 2),
        ] {
            sqlx::query(
                "INSERT INTO oc_filecache (fileid, storage, path, parent, name, mimetype) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(1)
            .bind(format!("files/{name}"))
            .bind(parent)
            .bind(name)
            .bind(mime)
            .execute(&pool)
            .await
            .expect("insert");
        }
        let batch = count_children_batch(&pool, prefix, &[2, 3, 4], 1, 2).await;
        assert_eq!(batch.get(&2), Some(&(1, 1)), "a: sub + x.txt");
        assert_eq!(batch.get(&3), None, "empty dir absent");
        assert_eq!(batch.get(&4), None, "file absent");
        for id in [2, 3, 4] {
            let single = count_children(&pool, prefix, id, 1, 2).await;
            assert_eq!(
                batch.get(&id).copied().unwrap_or((0, 0)),
                single,
                "fileid {id}"
            );
        }
    }

    #[tokio::test]
    async fn share_details_and_notes_batch_matches_singles() {
        let pool = fresh_batch_db().await;
        let prefix = "oc_";
        // alice owns files 10/11/12; bob shares his file 11 WITH alice.
        // (id, share_type, share_with, uid_owner, uid_initiator, file_source, stime, note)
        for (id, stype, swith, owner, init, fs, stime, note) in [
            (1, 0, "bob", "alice", "alice", 10, 100, ""), // detail, no note
            (2, 1, "staff", "alice", "alice", 10, 200, "staff-note"), // detail + note
            (3, 0, "erin", "alice", "alice", 10, 300, "erin-note"), // detail + most-recent note
            (4, 0, "alice", "bob", "bob", 11, 100, "bob-note"), // detail + note
            (5, 5, "x", "carol", "carol", 11, 500, "carol-note"), // outside details filter,
            // but the most-recent note on file 11 — notes must still see it
            (6, 0, "dave", "alice", "alice", 12, 500, ""), // detail, empty note at
            // the highest stime — must not hide the older note below
            (7, 0, "frank", "alice", "alice", 12, 400, "frank-note"), // detail + note
            (8, 1, "staff", "alice", "alice", 12, 300, ""),           // detail, no note
        ] {
            sqlx::query(
                "INSERT INTO oc_share (id, share_type, share_with, uid_owner, uid_initiator, file_source, stime, note) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(stype)
            .bind(swith)
            .bind(owner)
            .bind(init)
            .bind(fs)
            .bind(stime)
            .bind(note)
            .execute(&pool)
            .await
            .expect("insert");
        }
        sqlx::query("INSERT INTO oc_users (uid, displayname) VALUES (?, ?)")
            .bind("bob")
            .bind("Robert")
            .execute(&pool)
            .await
            .expect("user");
        let (details, notes) =
            share_details_and_notes_batch(&pool, prefix, "alice", &[10, 11, 12]).await;

        // Notes: max-stime non-empty note per file, filter-free (carol-note
        // lives on a share_type-5 row alice is not a party to).
        assert_eq!(notes.get(&10).map(String::as_str), Some("erin-note"));
        assert_eq!(notes.get(&11).map(String::as_str), Some("carol-note"));
        assert_eq!(notes.get(&12).map(String::as_str), Some("frank-note"));
        for id in [10, 11, 12] {
            assert_eq!(
                notes.get(&id).cloned().unwrap_or_default(),
                get_share_note(&pool, prefix, id).await,
                "note fileid {id}"
            );
        }

        // Details: same filter + display-name resolution as the single
        // query; SQL row order is unspecified, so compare as sorted sets.
        for id in [10, 11, 12] {
            let mut single = get_share_details(&pool, prefix, "alice", id).await;
            let mut batched = details.get(&id).cloned().unwrap_or_default();
            let key = |d: &ShareDetail| {
                (
                    d.share_type,
                    d.share_with.clone(),
                    d.share_with_displayname.clone(),
                )
            };
            single.sort_by_key(key);
            batched.sort_by_key(key);
            assert_eq!(batched.len(), single.len(), "fileid {id} len");
            for (b, s) in batched.iter().zip(single.iter()) {
                assert_eq!(b.share_type, s.share_type, "fileid {id} type");
                assert_eq!(b.share_with, s.share_with, "fileid {id} with");
                assert_eq!(
                    b.share_with_displayname, s.share_with_displayname,
                    "fileid {id} displayname"
                );
            }
        }
        // The type-5 row and carol as owner/initiator never reach details.
        assert!(details.get(&11).unwrap().iter().all(|d| d.share_type != 5));
        // bob's user-share resolves the oc_users displayname; unknown dave
        // falls back to the uid.
        let t10 = details.get(&10).unwrap();
        assert!(t10.iter().any(|d| d.share_with_displayname == "Robert"));
        assert!(details
            .get(&12)
            .unwrap()
            .iter()
            .any(|d| d.share_with_displayname == "dave"));
    }

    #[tokio::test]
    async fn comments_batches_match_singles() {
        let pool = fresh_batch_db().await;
        let prefix = "oc_";
        for (id, obj, actor, ts) in [
            (1, 10, "alice", "2024-01-01 10:00:00"),
            (2, 10, "bob", "2024-01-02 10:00:00"),
            (3, 10, "bob", "2024-01-03 10:00:00"),
            (4, 11, "alice", "2024-01-01 10:00:00"),
            (5, 12, "bob", "2024-01-05 10:00:00"),
        ] {
            sqlx::query(
                "INSERT INTO oc_comments (id, object_type, object_id, actor_type, actor_id, creation_timestamp) \
                 VALUES (?, 'files', ?, 'users', ?, ?)",
            )
            .bind(id)
            .bind(obj.to_string())
            .bind(actor)
            .bind(ts)
            .execute(&pool)
            .await
            .expect("insert");
        }
        // alice has read file 10 up to the day-02 marker: bob's day-03
        // comment is unread; her own comments are excluded either way.
        sqlx::query(
            "INSERT INTO oc_comments_read_markers (user_id, object_type, object_id, marker_datetime) \
             VALUES (?, 'files', ?, ?)",
        )
        .bind("alice")
        .bind("10")
        .bind("2024-01-02 10:00:00")
        .execute(&pool)
        .await
        .expect("marker");
        let merged = comments_counts_batch(&pool, prefix, &[10, 11, 12], "alice").await;
        for id in [10, 11, 12] {
            let (c, u) = merged.get(&id).copied().unwrap_or((0, 0));
            assert_eq!(c, get_comments_count(&pool, prefix, id).await, "count {id}");
            assert_eq!(
                u,
                get_comments_unread(&pool, prefix, id, "alice").await,
                "unread {id}"
            );
        }
        assert_eq!(
            merged.get(&10),
            Some(&(3, 1)),
            "file 10: 3 comments, bob@day03 unread"
        );
        assert_eq!(
            merged.get(&11),
            Some(&(1, 0)),
            "file 11: alice's own comment, nothing unread"
        );
        assert_eq!(
            merged.get(&12),
            Some(&(1, 1)),
            "file 12: bob's comment, no marker"
        );
    }

    #[tokio::test]
    async fn system_tags_batch_matches_single() {
        let pool = fresh_batch_db().await;
        let prefix = "oc_";
        for (id, name, vis) in [(1, "Beta", 1), (2, "alpha", 1), (3, "hidden", 0)] {
            sqlx::query(
                "INSERT INTO oc_systemtag (id, name, visibility, editable) VALUES (?, ?, ?, 1)",
            )
            .bind(id)
            .bind(name)
            .bind(vis)
            .execute(&pool)
            .await
            .expect("tag");
        }
        for (obj, tag) in [("10", 1), ("10", 2), ("11", 3)] {
            sqlx::query(
                "INSERT INTO oc_systemtag_object_mapping (objectid, objecttype, systemtagid) \
                 VALUES (?, 'files', ?)",
            )
            .bind(obj)
            .bind(tag)
            .execute(&pool)
            .await
            .expect("map");
        }
        let batch = system_tags_batch(&pool, prefix, &[10, 11]).await;
        for id in [10, 11] {
            let b = batch.get(&id).cloned().unwrap_or_default();
            let s = get_system_tags_for_file(&pool, prefix, id).await;
            assert_eq!(b.len(), s.len(), "len {id}");
            for (x, y) in b.iter().zip(s.iter()) {
                assert_eq!(x.id, y.id, "id {id}");
                assert_eq!(x.name, y.name, "name {id}");
            }
        }
        // file 10: alpha then Beta (LOWER-sorted), hidden tag excluded; file
        // 11's only tag is hidden → absent from the batch map.
        let t10 = batch.get(&10).unwrap();
        assert_eq!(
            t10.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "Beta"]
        );
        assert!(batch.get(&11).is_none());
    }

    #[tokio::test]
    async fn custom_properties_batch_matches_single() {
        let pool = fresh_batch_db().await;
        let prefix = "oc_";
        for (p, name, val) in [
            ("files/a.txt", "{urn:x}one", "<v>1</v>"),
            ("files/a.txt", "{urn:x}two", "<v>2</v>"),
            ("files/b.txt", "{urn:x}one", "<v>3</v>"),
        ] {
            upsert_custom_property(&pool, prefix, "alice", p, name, val.as_bytes(), 1)
                .await
                .expect("upsert");
        }
        let paths = vec![
            "files/a.txt".to_string(),
            "files/b.txt".to_string(),
            "files/c.txt".to_string(),
        ];
        let batch = custom_properties_batch(&pool, prefix, "alice", &paths).await;
        for p in ["files/a.txt", "files/b.txt", "files/c.txt"] {
            let b = batch.get(p).cloned().unwrap_or_default();
            let s = list_custom_properties(&pool, prefix, "alice", p).await;
            assert_eq!(b, s, "{p}");
        }
        assert_eq!(batch.get("files/a.txt").unwrap().len(), 2);
        assert!(batch.get("files/c.txt").is_none());
    }

    // ── oc_properties CRUD smoke test (SQLite in-memory) ─────────────────

    async fn fresh_props_db() -> DbPool {
        let pool = DbPool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("in-memory SQLite"),
        );
        // Create the table matching 0013_properties.sql
        sqlx::query(
            "CREATE TABLE oc_properties (
                id            INTEGER NOT NULL PRIMARY KEY,
                userid        VARCHAR(64) NOT NULL DEFAULT '',
                propertypath  VARCHAR(255) NOT NULL DEFAULT '',
                propertyname  VARCHAR(255) NOT NULL DEFAULT '',
                propertyvalue TEXT NOT NULL DEFAULT '',
                valuetype     SMALLINT NOT NULL DEFAULT 1
            )",
        )
        .execute(&pool)
        .await
        .expect("create table");
        sqlx::query("CREATE INDEX IF NOT EXISTS properties_path_uid ON oc_properties (userid, propertypath)")
            .execute(&pool)
            .await
            .expect("create index");
        pool
    }

    #[tokio::test]
    async fn custom_props_roundtrip_upsert_and_list() {
        let pool = fresh_props_db().await;
        let prefix = "oc_";
        let xml = b"<ok xmlns=\"urn:example\">hello</ok>";

        // Insert
        upsert_custom_property(
            &pool,
            prefix,
            "alice",
            "files/notes.txt",
            "{urn:example}state",
            xml,
            2,
        )
        .await
        .expect("upsert");

        // Read back
        let props = list_custom_properties(&pool, prefix, "alice", "files/notes.txt").await;
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].0, "{urn:example}state");
        assert_eq!(props[0].1, "<ok xmlns=\"urn:example\">hello</ok>");
        assert_eq!(props[0].2, 2);
    }

    #[tokio::test]
    async fn custom_props_upsert_overwrites_existing() {
        let pool = fresh_props_db().await;
        let prefix = "oc_";

        // First write
        upsert_custom_property(
            &pool,
            prefix,
            "alice",
            "files/x.txt",
            "{urn:a}v",
            b"<a/>",
            2,
        )
        .await
        .expect("upsert 1");

        // Second write with different value
        upsert_custom_property(
            &pool,
            prefix,
            "alice",
            "files/x.txt",
            "{urn:a}v",
            b"<b/>",
            2,
        )
        .await
        .expect("upsert 2");

        // Should have exactly one row with the latest value
        let props = list_custom_properties(&pool, prefix, "alice", "files/x.txt").await;
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].1, "<b/>");
    }

    #[tokio::test]
    async fn custom_props_delete_single() {
        let pool = fresh_props_db().await;
        let prefix = "oc_";

        upsert_custom_property(
            &pool,
            prefix,
            "alice",
            "files/a.txt",
            "{urn:x}p",
            b"<p/>",
            2,
        )
        .await
        .expect("upsert");

        // Delete it
        delete_custom_property(&pool, prefix, "alice", "files/a.txt", "{urn:x}p")
            .await
            .expect("delete");

        let props = list_custom_properties(&pool, prefix, "alice", "files/a.txt").await;
        assert_eq!(props.len(), 0);
    }

    #[tokio::test]
    async fn custom_props_delete_path_removes_all() {
        let pool = fresh_props_db().await;
        let prefix = "oc_";

        upsert_custom_property(
            &pool,
            prefix,
            "alice",
            "files/b.txt",
            "{urn:a}p1",
            b"<p1/>",
            2,
        )
        .await
        .expect("upsert p1");
        upsert_custom_property(
            &pool,
            prefix,
            "alice",
            "files/b.txt",
            "{urn:a}p2",
            b"<p2/>",
            2,
        )
        .await
        .expect("upsert p2");

        // Delete all for this path
        delete_custom_properties_for_path(&pool, prefix, "alice", "files/b.txt")
            .await
            .expect("delete path");

        let props = list_custom_properties(&pool, prefix, "alice", "files/b.txt").await;
        assert_eq!(props.len(), 0);
    }

    #[tokio::test]
    async fn custom_props_user_isolation() {
        let pool = fresh_props_db().await;
        let prefix = "oc_";

        upsert_custom_property(
            &pool,
            prefix,
            "alice",
            "files/shared.txt",
            "{urn:x}p",
            b"<alice/>",
            2,
        )
        .await
        .expect("upsert alice");
        upsert_custom_property(
            &pool,
            prefix,
            "bob",
            "files/shared.txt",
            "{urn:x}p",
            b"<bob/>",
            2,
        )
        .await
        .expect("upsert bob");

        let alice_props = list_custom_properties(&pool, prefix, "alice", "files/shared.txt").await;
        assert_eq!(alice_props.len(), 1);
        assert_eq!(alice_props[0].1, "<alice/>");

        let bob_props = list_custom_properties(&pool, prefix, "bob", "files/shared.txt").await;
        assert_eq!(bob_props.len(), 1);
        assert_eq!(bob_props[0].1, "<bob/>");
    }

    #[tokio::test]
    async fn custom_props_path_format_hashes_long_paths() {
        let pool = fresh_props_db().await;
        let prefix = "oc_";
        let long_path = "files/".to_string() + &"d".repeat(260);

        upsert_custom_property(&pool, prefix, "alice", &long_path, "{urn:x}p", b"<p/>", 2)
            .await
            .expect("upsert long");

        // The stored path should be hashed, but lookups use the same hash so
        // it round-trips correctly.
        let props = list_custom_properties(&pool, prefix, "alice", &long_path).await;
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].1, "<p/>");
    }

    // ── Phase 12.3 / 12.4: permission masking pipeline ──────────────────────
    //
    // The only SHARE-bit stripping Rust performs is `apply_sharing_mask`, which
    // mirrors PHP's SetupManager `sharing_mask` storage wrapper and fires ONLY
    // when sharing is disabled via shareapi config.  In the normal (sharing
    // enabled) case permissions pass through unchanged, so the home storage
    // root keeps PERMISSION_SHARE — verified against live PHP: the home root
    // reports `oc:permissions` = RGDNVCK, `ocs:share-permissions` = 31,
    // `ocm:share-permissions` = ["share","read","write"].  (An earlier revision
    // also stripped SHARE unconditionally on the mount root to match a stale
    // cold-start capture; that strip was removed — see filesystem.rs.)

    const P_READ: i32 = 1;
    const P_UPDATE: i32 = 2;
    const P_CREATE: i32 = 4;
    const P_DELETE: i32 = 8;
    const P_SHARE: i32 = 16;
    const P_ALL: i32 = 31;

    #[test]
    fn apply_sharing_mask_passthrough_when_sharing_enabled() {
        // Master environment: shareapi_exclude_groups unset → sharing enabled →
        // the SetupManager sharing_mask wrapper is inactive, permissions pass
        // through unchanged.
        assert_eq!(apply_sharing_mask(P_ALL, false), P_ALL);
    }

    #[test]
    fn apply_sharing_mask_strips_share_when_disabled() {
        // When sharing is disabled for the user, PermissionsMask(mask=15) wraps
        // the cache layer and strips PERMISSION_SHARE from every read.
        assert_eq!(apply_sharing_mask(P_ALL, true), P_ALL - P_SHARE);
        assert_eq!(apply_sharing_mask(P_ALL, true), 15);
    }

    #[test]
    fn compute_share_permissions_mount_root_dir() {
        // A mount root whose effective permissions are 15 (SHARE absent — the
        // sharing-disabled case).  The mount-root DELETE|UPDATE OR-in is a no-op
        // (both bits already set).
        assert_eq!(compute_share_permissions(15, true, true), 15);
    }

    #[test]
    fn compute_share_permissions_non_root_dir() {
        // Ordinary directory keeps its full permissions (SHARE included).
        assert_eq!(compute_share_permissions(P_ALL, true, false), P_ALL);
    }

    #[test]
    fn compute_share_permissions_mount_root_gains_delete_update() {
        // A mount root that somehow lacked DELETE|UPDATE gains them (PHP
        // Node::getSharePermissions lines 261-275).
        let read_only = P_READ | P_CREATE; // 5
        assert_eq!(
            compute_share_permissions(read_only, true, true),
            P_READ | P_CREATE | P_DELETE | P_UPDATE
        );
    }

    #[test]
    fn compute_share_permissions_file_strips_create_delete() {
        // Files can never carry CREATE or DELETE (PHP lines 280-282).
        assert_eq!(
            compute_share_permissions(P_ALL, false, false),
            P_ALL & !(P_CREATE | P_DELETE)
        );
        assert_eq!(compute_share_permissions(P_ALL, false, false), 19);
    }

    #[test]
    fn permissions_to_ocm_json_without_share() {
        // 15 has no SHARE bit → "share" is dropped from the OCM array (PHP
        // FilesPlugin::ncPermissions2ocmPermissions).
        assert_eq!(permissions_to_ocm_json(15), r#"["read","write"]"#);
    }

    #[test]
    fn permissions_to_ocm_json_with_share() {
        assert_eq!(
            permissions_to_ocm_json(P_ALL),
            r#"["share","read","write"]"#
        );
    }

    #[test]
    fn home_root_permission_pipeline_matches_php() {
        // End-to-end composition for the home storage root, mirroring
        // `filesystem.rs::get_props`.  DB stores 31 and — sharing enabled — the
        // value passes through unchanged.  Verified against live PHP: the home
        // root returns RGDNVCK / ocs=31 / ocm=["share","read","write"].
        let db_permissions = P_ALL;
        let sharing_disabled = false; // master environment
        let is_mount_root = true;

        let effective = apply_sharing_mask(db_permissions, sharing_disabled);

        assert_eq!(effective, P_ALL, "home root keeps SHARE (→ RGDNVCK)");
        assert_eq!(
            compute_share_permissions(effective, true, is_mount_root),
            P_ALL
        );
        assert_eq!(
            permissions_to_ocm_json(effective),
            r#"["share","read","write"]"#
        );
    }

    #[test]
    fn home_root_permission_pipeline_strips_share_when_sharing_disabled() {
        // When sharing is genuinely disabled (shareapi config), the mask strips
        // SHARE even on the home root → GDNVCK / ocs=15 / ocm=["read","write"].
        let effective = apply_sharing_mask(P_ALL, true);
        assert_eq!(effective, P_ALL - P_SHARE);
        assert_eq!(permissions_to_ocm_json(effective), r#"["read","write"]"#);
    }

    #[test]
    fn non_root_dir_permission_pipeline_matches_php() {
        // Ordinary directory (e.g. "files/Photos"): SHARE is retained.  PHP
        // returns RGDNVCK / ocs=31 / ocm=["share","read","write"].
        let db_permissions = P_ALL;
        let sharing_disabled = false;
        let is_mount_root = false;

        let effective = apply_sharing_mask(db_permissions, sharing_disabled);

        assert_eq!(effective, P_ALL, "non-root keeps SHARE (→ RGDNVCK)");
        assert_eq!(
            compute_share_permissions(effective, true, is_mount_root),
            P_ALL
        );
        assert_eq!(
            permissions_to_ocm_json(effective),
            r#"["share","read","write"]"#
        );
    }
}
