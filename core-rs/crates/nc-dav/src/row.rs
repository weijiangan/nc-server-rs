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
        match sqlx::query(&sql)
            .bind(&adjusted)
            .fetch_optional(pool)
            .await
        {
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

/// Look up the display name for a user, matching PHP's
/// `$owner->getDisplayName()` → `IAccountManager` → `oc_accounts.data`,
/// falling back to `oc_users.displayname`, and finally to the UID itself.
///
/// PHP's `oc_accounts` table stores account data as JSON in the `data` column:
/// `{"displayname":{"value":"Tan Siew Kin",...},...}`.  The display name is
/// resolved from `data->>'displayname'->>'value'` first, then from
/// `oc_users.displayname` as a fallback.
pub async fn lookup_user_display_name(pool: &DbPool, prefix: &str, uid: &str) -> String {
    // 1. Try oc_accounts first (PHP IAccountManager path).
    let accounts_sql = format!("SELECT data FROM {prefix}accounts WHERE uid = $1");
    let accounts_data: Option<String> = sqlx::query_scalar(&accounts_sql)
        .bind(uid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    if let Some(ref data) = accounts_data {
        if let Some(dn) = extract_displayname_from_accounts_json(data) {
            return dn;
        }
    }

    // 2. Fall back to oc_users.displayname.
    let users_sql = format!("SELECT displayname FROM {prefix}users WHERE uid = $1");
    if let Ok(Some(dn)) = sqlx::query_scalar::<_, Option<String>>(&users_sql)
        .bind(uid)
        .fetch_optional(pool)
        .await
    {
        if let Some(dn) = dn {
            if !dn.is_empty() {
                return dn;
            }
        }
    }

    // 3. Fall back to UID.
    uid.to_string()
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
    let sql = format!(
        "DELETE FROM {prefix}properties WHERE userid=$1 AND propertypath=$2"
    );
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

/// Check whether sharing is disabled for a user, replicating PHP
/// `ShareDisableChecker::sharingDisabledForUser()`.
///
/// Reads `shareapi_exclude_groups` and `shareapi_exclude_groups_list` from
/// `oc_appconfig`, then checks the user's group membership in `oc_group_user`.
///
/// PHP's `sharing_mask` storage wrapper (`SetupManager.php:176-189`) wraps
/// storages with `PermissionsMask(mask=PERMISSION_ALL-SHARE=15)` whenever this
/// returns `true`, stripping the SHARE bit from every cache read.  Rust must
/// replicate this check so permissions match PHP byte-for-byte.
pub async fn sharing_disabled_for_user(pool: &DbPool, prefix: &str, uid: &str) -> bool {
    // Read shareapi_exclude_groups from oc_appconfig.
    let key = "shareapi_exclude_groups";
    let sql = format!(
        "SELECT configvalue FROM {prefix}appconfig WHERE appid = 'core' AND configkey = $1"
    );
    let exclude_groups: Option<String> = sqlx::query_scalar(&sql)
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

    match exclude_groups.as_deref() {
        None | Some("no") | Some("") => {
            // Sharing is not restricted by group — enabled for everyone.
            false
        }
        Some(mode @ ("yes" | "allow")) => {
            // Read the group list.
            let list_sql = format!(
                "SELECT configvalue FROM {prefix}appconfig WHERE appid = 'core' AND configkey = 'shareapi_exclude_groups_list'"
            );
            let list_val: Option<String> = sqlx::query_scalar(&list_sql)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();

            let excluded_groups: Vec<String> = match list_val.as_deref() {
                Some(s) if !s.is_empty() => {
                    // PHP tries json_decode first, then explodes on comma.
                    serde_json::from_str::<Vec<String>>(s)
                        .unwrap_or_else(|_| s.split(',').map(|g| g.trim().to_string()).collect())
                }
                _ => vec![],
            };

            if excluded_groups.is_empty() {
                return false;
            }

            // Query the user's group memberships.
            let groups_sql = format!("SELECT gid FROM {prefix}group_user WHERE uid = $1");
            let user_groups: Vec<String> = match sqlx::query_scalar::<_, String>(&groups_sql)
                .bind(uid)
                .fetch_all(pool)
                .await
            {
                Ok(g) => g,
                Err(_) => return false,
            };

            if mode == "allow" {
                // Allowlist: sharing allowed only if user is in at least one allowed group.
                // If user is in no groups at all, they can't be in an allowed group → disabled.
                let in_allowed = user_groups.iter().any(|g| excluded_groups.contains(g));
                !in_allowed
            } else {
                // Exclude mode: sharing disabled only if ALL user groups are excluded.
                // PHP: if (!empty($usersGroups)) guards the diff; empty groups → falls
                // through to return false (sharing NOT disabled).
                if user_groups.is_empty() {
                    false
                } else {
                    user_groups.iter().all(|g| excluded_groups.contains(g))
                }
            }
        }
        Some(other) => {
            tracing::warn!(
                %other,
                "unexpected value for shareapi_exclude_groups; treating as sharing enabled"
            );
            false
        }
    }
}

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

    // 1. Batch-query oc_accounts first (PHP IAccountManager path).
    //    oc_accounts stores display names in JSON under data->'displayname'->>'value'.
    let placeholders = uids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let accounts_sql = format!(
        "SELECT uid, data FROM {prefix}accounts WHERE uid IN ({placeholders})",
        prefix = prefix
    );
    let mut query = sqlx::query(&accounts_sql);
    for uid in uids {
        query = query.bind(uid);
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
            display_names.insert(uid.clone(), dn);
        }
    }

    // Collect UIDs not found in oc_accounts for the oc_users fallback.
    for uid in uids {
        if !display_names.contains_key(uid.as_str()) {
            unresolved.push(uid);
        }
    }

    // 2. Batch-query oc_users for remaining UIDs.
    if !unresolved.is_empty() {
        let users_placeholders = unresolved
            .iter()
            .enumerate()
            .map(|(i, _)| format!("${}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let users_sql = format!(
            "SELECT uid, displayname FROM {prefix}users WHERE uid IN ({users_placeholders})",
            prefix = prefix
        );
        let mut query = sqlx::query(&users_sql);
        for uid in &unresolved {
            query = query.bind(uid);
        }
        let user_rows = query.fetch_all(pool).await.unwrap_or_default();
        for row in &user_rows {
            let uid: String = row.get("uid");
            let dn: Option<String> = row.get("displayname");
            if let Some(dn) = dn.filter(|s| !s.is_empty()) {
                display_names.entry(uid).or_insert(dn);
            }
        }
        // 3. For any UIDs still unresolved, fall back to the UID itself.
        for uid in &unresolved {
            display_names.entry((*uid).clone()).or_insert_with(|| (*uid).clone());
        }
    }

    // 4. For UIDs that had no oc_accounts row and no oc_users row, fall back to UID.
    for uid in uids {
        display_names.entry(uid.clone()).or_insert_with(|| uid.clone());
    }

    display_names
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
pub async fn get_comments_unread(
    pool: &DbPool,
    prefix: &str,
    fileid: i64,
    uid: &str,
) -> i64 {
    let sql = format!(
        "SELECT COUNT(*) FROM {prefix}comments c \
         WHERE c.object_type = 'files' AND c.object_id = $1 \
         AND c.actor_type = 'users' AND c.actor_id != $2 \
         AND c.creation_timestamp > COALESCE( \
             (SELECT marker_datetime FROM {prefix}comments_read_markers \
              WHERE user_id = $3 AND object_type = 'files' AND object_id = $4), \
             TIMESTAMP '1970-01-01 00:00:00' \
         )",
        prefix = prefix
    );
    sqlx::query_scalar::<_, Option<i64>>(&sql)
        .bind(fileid.to_string())
        .bind(uid)
        .bind(uid)
        .bind(fileid.to_string())
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or(0)
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

    // ── oc_properties CRUD smoke test (SQLite in-memory) ─────────────────

    async fn fresh_props_db() -> DbPool {
        sqlx::any::install_default_drivers();
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite");
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
        upsert_custom_property(&pool, prefix, "alice", "files/notes.txt", "{urn:example}state", xml, 2)
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
        upsert_custom_property(&pool, prefix, "alice", "files/x.txt", "{urn:a}v", b"<a/>", 2)
            .await
            .expect("upsert 1");

        // Second write with different value
        upsert_custom_property(&pool, prefix, "alice", "files/x.txt", "{urn:a}v", b"<b/>", 2)
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

        upsert_custom_property(&pool, prefix, "alice", "files/a.txt", "{urn:x}p", b"<p/>", 2)
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

        upsert_custom_property(&pool, prefix, "alice", "files/b.txt", "{urn:a}p1", b"<p1/>", 2)
            .await
            .expect("upsert p1");
        upsert_custom_property(&pool, prefix, "alice", "files/b.txt", "{urn:a}p2", b"<p2/>", 2)
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

        upsert_custom_property(&pool, prefix, "alice", "files/shared.txt", "{urn:x}p", b"<alice/>", 2)
            .await
            .expect("upsert alice");
        upsert_custom_property(&pool, prefix, "bob", "files/shared.txt", "{urn:x}p", b"<bob/>", 2)
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
}
