use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use sqlx::{Postgres, Row, Sqlite};
use super::paths::path_hash;
use super::types::{FileCacheExtRow, FileCacheRow};


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
    let fetched: Result<Option<FileCacheRow>, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query::<Postgres>(&sql)
            .bind(storage)
            .bind(&hash)
            .fetch_optional(p)
            .await
            .map(|r| r.map(|r| fc_row_from_pg(&r))),
        DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql)
            .bind(storage)
            .bind(&hash)
            .fetch_optional(p)
            .await
            .map(|r| r.map(|r| fc_row_from_sqlite(&r))),
    };
    match &fetched {
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
                let debug_rows: Vec<(i64, i64, Option<String>)> = db_dispatch!(pool, |Db, c| {
                    sqlx::query::<Db>(&debug_sql)
                        .bind(&hash)
                        .fetch_all(c)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|r| (r.get(0), r.get(1), r.get(2)))
                        .collect()
                });
                tracing::trace!(
                    path = %path, hash = %hash, storage, ?debug_rows,
                    "lookup_by_path: not found (any storage)"
                );
            }
        }
    }
    match fetched {
        Err(_) => None,
        Ok(row) => row,
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
    let fetched: Result<Option<(FileCacheRow, FileCacheExtRow)>, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query::<Postgres>(&sql)
            .bind(storage)
            .bind(&hash)
            .fetch_optional(p)
            .await
            .map(|r| {
                r.map(|r| {
                    let row = fc_row_from_pg(&r);
                    let ext = FileCacheExtRow {
                        metadata_etag: r.get::<Option<String>, _>("metadata_etag"),
                        creation_time: r.get::<Option<i64>, _>("creation_time").unwrap_or(0),
                        upload_time: r.get::<Option<i64>, _>("upload_time").unwrap_or(0),
                    };
                    (row, ext)
                })
            }),
        DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql)
            .bind(storage)
            .bind(&hash)
            .fetch_optional(p)
            .await
            .map(|r| {
                r.map(|r| {
                    let row = fc_row_from_sqlite(&r);
                    let ext = FileCacheExtRow {
                        metadata_etag: r.get::<Option<String>, _>("metadata_etag"),
                        creation_time: r.get::<Option<i64>, _>("creation_time").unwrap_or(0),
                        upload_time: r.get::<Option<i64>, _>("upload_time").unwrap_or(0),
                    };
                    (row, ext)
                })
            }),
    };
    match fetched {
        Ok(Some(v)) => Some(v),
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
    let fetched: Result<Option<FileCacheRow>, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query::<Postgres>(&sql)
            .bind(fileid)
            .fetch_optional(p)
            .await
            .map(|r| r.map(|r| fc_row_from_pg(&r))),
        DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql)
            .bind(fileid)
            .fetch_optional(p)
            .await
            .map(|r| r.map(|r| fc_row_from_sqlite(&r))),
    };
    match fetched {
        Err(e) => {
            tracing::error!(error = %e, fileid = fileid, "lookup_by_id: SQL error");
            None
        }
        Ok(row) => row,
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
    let fetched: Result<Vec<FileCacheRow>, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query::<Postgres>(&sql)
            .bind(parent_id)
            .bind(storage)
            .fetch_all(p)
            .await
            .map(|rows| rows.iter().map(fc_row_from_pg).collect()),
        DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql)
            .bind(parent_id)
            .bind(storage)
            .fetch_all(p)
            .await
            .map(|rows| rows.iter().map(fc_row_from_sqlite).collect()),
    };
    match fetched {
        Err(e) => {
            tracing::error!(error = %e, parent_id = parent_id, "list_children: SQL error");
            Vec::new()
        }
        Ok(rows) => rows,
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
    let fetched: Result<Vec<(FileCacheRow, FileCacheExtRow)>, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query::<Postgres>(&sql)
            .bind(parent_id)
            .bind(storage)
            .fetch_all(p)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|r| {
                        let row = fc_row_from_pg(r);
                        let ext = FileCacheExtRow {
                            metadata_etag: r.get::<Option<String>, _>("metadata_etag"),
                            creation_time: r.get::<Option<i64>, _>("creation_time").unwrap_or(0),
                            upload_time: r.get::<Option<i64>, _>("upload_time").unwrap_or(0),
                        };
                        (row, ext)
                    })
                    .collect()
            }),
        DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql)
            .bind(parent_id)
            .bind(storage)
            .fetch_all(p)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|r| {
                        let row = fc_row_from_sqlite(r);
                        let ext = FileCacheExtRow {
                            metadata_etag: r.get::<Option<String>, _>("metadata_etag"),
                            creation_time: r.get::<Option<i64>, _>("creation_time").unwrap_or(0),
                            upload_time: r.get::<Option<i64>, _>("upload_time").unwrap_or(0),
                        };
                        (row, ext)
                    })
                    .collect()
            }),
    };
    match fetched {
        Err(e) => {
            tracing::error!(error = %e, parent_id = parent_id, "list_children_with_ext: SQL error");
        }
        Ok(rows) => {
            for (row, ext) in rows {
                ext_map.insert(row.fileid, ext);
                rows_out.push(row);
            }
        }
    }
    (rows_out, ext_map)
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
    let fetched: Result<Vec<FileCacheRow>, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query::<Postgres>(&sql)
            .bind(storage_id)
            .bind(fc_path)
            .bind(&like_pat)
            .bind(since_mtime)
            .fetch_all(p)
            .await
            .map(|rows| rows.iter().map(fc_row_from_pg).collect()),
        DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql)
            .bind(storage_id)
            .bind(fc_path)
            .bind(&like_pat)
            .bind(since_mtime)
            .fetch_all(p)
            .await
            .map(|rows| rows.iter().map(fc_row_from_sqlite).collect()),
    };
    match fetched {
        Err(e) => {
            tracing::error!(error = %e, fc_path = %fc_path, "list_changed_since: SQL error");
            Vec::new()
        }
        Ok(rows) => rows,
    }
}


// ─── private helper ───────────────────────────────────────────────────────────

pub(crate) fn fc_row_from_sqlite(r: &sqlx::sqlite::SqliteRow) -> FileCacheRow {
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
        creation_time: 0,
        upload_time: 0,
    }
}


pub(crate) fn fc_row_from_pg(r: &sqlx::postgres::PgRow) -> FileCacheRow {
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
        creation_time: 0,
        upload_time: 0,
    }
}
