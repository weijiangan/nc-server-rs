use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use sqlx::{Postgres, Row};
use super::types::FileCacheExtRow;


/// Fetch extended metadata for a file (creation_time, upload_time, metadata_etag).
pub async fn get_extended(pool: &DbPool, prefix: &str, fileid: i64) -> FileCacheExtRow {
    let sql = format!(
        "SELECT metadata_etag, creation_time, upload_time \
         FROM {prefix}filecache_extended WHERE fileid = $1"
    );
    db_dispatch!(pool, |Db, c| {
        sqlx::query::<Db>(&sql)
            .bind(fileid)
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
            .map(|r| FileCacheExtRow {
                metadata_etag: r.get("metadata_etag"),
                creation_time: r.get::<i64, _>("creation_time"),
                upload_time: r.get::<i64, _>("upload_time"),
            })
            .unwrap_or_default()
    })
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

    // Native bigint[] bind on Postgres (PHASE-22 T4).
    match pool {
        DbPool::Pg(p) => {
            let sql = format!(
                "SELECT fileid, metadata_etag, creation_time, upload_time \
                 FROM {prefix}filecache_extended \
                 WHERE fileid = ANY($1::bigint[])",
                prefix = prefix,
            );
            sqlx::query::<Postgres>(&sql)
                .bind(fileids)
                .fetch_all(p)
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
        DbPool::Sqlite(p) => {
            let placeholders = (1..=fileids.len())
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT fileid, metadata_etag, creation_time, upload_time \
                 FROM {prefix}filecache_extended WHERE fileid IN ({placeholders})",
                prefix = prefix,
            );
            let mut query = sqlx::query(&sql);
            for id in fileids {
                query = query.bind(*id);
            }
            query
                .fetch_all(p)
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
    }
}
