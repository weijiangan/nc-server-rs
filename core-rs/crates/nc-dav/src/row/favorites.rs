use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use sqlx::Postgres;
use super::filecache::{fc_row_from_pg, fc_row_from_sqlite};
use super::types::FileCacheRow;


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
    let fetched = db_dispatch!(pool, |Db, c| {
        sqlx::query_scalar::<Db, String>(&sql)
            .bind(uid)
            .bind(crate::tags::TAG_FAVORITE)
            .fetch_all(c)
            .await
    });
    match fetched {
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
    match pool {
        DbPool::Pg(p) => {
            let sql = format!(
                "SELECT fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
                 size, mtime, storage_mtime, etag, permissions, checksum, \
                 creation_time, upload_time \
                 FROM {prefix}filecache WHERE fileid = ANY($1::bigint[])",
                prefix = prefix
            );
            sqlx::query::<Postgres>(&sql)
                .bind(fileids)
                .fetch_all(p)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    let row = fc_row_from_pg(&r);
                    (row.fileid, row)
                })
                .collect()
        }
        DbPool::Sqlite(p) => {
            let placeholders = fileids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("${}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
             size, mtime, storage_mtime, etag, permissions, checksum, \
             creation_time, upload_time \
             FROM {prefix}filecache WHERE fileid IN ({placeholders})",
                prefix = prefix
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
                    let row = fc_row_from_sqlite(&r);
                    (row.fileid, row)
                })
                .collect()
        }
    }
}
