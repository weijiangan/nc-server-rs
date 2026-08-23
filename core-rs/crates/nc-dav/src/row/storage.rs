use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use sqlx::Row;


// ─── Storage ─────────────────────────────────────────────────────────────────

/// Look up the `numeric_id` for a user's home storage.
///
/// Tries multiple known formats:
/// - `home::{uid}` — used by NC home directory wrappers (default for most installs)
/// - `local::{data_dir}/{uid}/` — used by raw LocalStorage backends
/// Match PHP's `\OC\Files\Cache\Storage::adjustStorageId`.
/// Storage IDs longer than 64 characters are stored as their MD5 hex digest
/// in `oc_storages.id` (see `lib/private/Files/Cache/Storage.php:99-105`).
pub(crate) fn adjust_storage_id(id: &str) -> String {
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
        let fetched: Result<Option<i64>, sqlx::Error> = db_dispatch!(pool, |Db, c| {
            sqlx::query::<Db>(&sql)
                .bind(&adjusted)
                .fetch_optional(c)
                .await
                .map(|r| r.map(|row| row.get::<i64, _>("numeric_id")))
        });
        match fetched {
            Ok(Some(id)) => return Some(id),
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


/// Look up the string ID for a storage row by its numeric ID.
///
/// Used in `get_props()` to determine whether a file's storage is a home
/// storage (`id` starts with `"home::"`) for the `M` (mounted) flag in
/// `{oc:}permissions` (PHASE-7.6).  Returns `None` when the storage row does
/// not exist.
pub async fn get_storage_string_id(pool: &DbPool, prefix: &str, numeric_id: i64) -> Option<String> {
    let sql = format!("SELECT id FROM {prefix}storages WHERE numeric_id = $1");
    db_dispatch!(pool, |Db, c| {
        sqlx::query_scalar::<Db, Option<String>>(&sql)
            .bind(numeric_id)
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
            .flatten()
    })
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
