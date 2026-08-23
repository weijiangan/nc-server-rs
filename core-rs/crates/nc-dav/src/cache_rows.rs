use crate::path_utils::new_etag;
use crate::row;
use crate::NcDavState;
use nc_db::db_execute;
use nc_db::mime::SharedMimeCache;
use nc_db::pool::DbPool;

/// Materialize the install-created `cache/` row once per storage process.
pub(crate) async fn ensure_lazy_cache_row(state: &NcDavState, storage_id: i64, now: i64) {
    if state
        .lazy_cache_ensured
        .lock()
        .expect("lazy cache set")
        .contains(&storage_id)
    {
        tracing::debug!(storage_id, "lazy-cache: already ensured this process");
        return;
    }
    if row::lookup_by_path(&state.pool, &state.table_prefix, storage_id, "cache")
        .await
        .is_some()
    {
        tracing::debug!(storage_id, "lazy-cache: row already present");
        state
            .lazy_cache_ensured
            .lock()
            .expect("lazy cache set")
            .insert(storage_id);
        return;
    }
    tracing::debug!(storage_id, "lazy-cache: materializing row");
    ensure_lazy_dir_row(
        &state.pool,
        &state.table_prefix,
        storage_id,
        &state.mime_cache,
        "cache",
        now,
    )
    .await;
    let etag = new_etag();
    let sql = format!(
        "UPDATE {prefix}filecache SET etag = $1, storage_mtime = GREATEST(storage_mtime + 60, $2) \
         WHERE storage = $3 AND path = ''",
        prefix = state.table_prefix,
    );
    let result = db_execute!(&state.pool, &sql, &etag, now, storage_id);
    if let Err(e) = result {
        tracing::warn!(error = %e, "lazy cache row: storage-root bump failed");
    }
    state
        .lazy_cache_ensured
        .lock()
        .expect("lazy cache set")
        .insert(storage_id);
}

/// Lazily register the `core | files_metadata` appconfig row.
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
    let result = db_execute!(pool, &sql, &config_value);
    if let Err(e) = result {
        tracing::warn!(error = %e, "files_metadata appconfig registration failed");
    }
}

/// Materialize a top-level directory row if it is absent.
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
    let dir_mimepart_id =
        nc_db::mime::get_or_insert_mime_id(pool, prefix, mime_cache, "httpd").await;
    let parent_id = row::lookup_by_path(pool, prefix, storage_id, "")
        .await
        .map(|r| r.fileid)
        .unwrap_or(-1);
    let dir_hash = row::path_hash(dir_name);
    let dir_etag = new_etag();
    let sql = format!(
        "INSERT INTO {prefix}filecache \
         (storage, path, path_hash, parent, name, mimetype, mimepart, \
          size, mtime, storage_mtime, etag, permissions, checksum) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
         ON CONFLICT DO NOTHING",
        prefix = prefix
    );
    let result = db_execute!(
        pool,
        &sql,
        storage_id,
        dir_name,
        &dir_hash,
        parent_id,
        dir_name,
        dir_mime_id,
        dir_mimepart_id,
        0i64,
        now,
        now,
        &dir_etag,
        31i32,
        ""
    );
    if let Err(e) = result {
        tracing::warn!(dir = dir_name, error = %e, "lazy dir row materialization failed");
    }
}
