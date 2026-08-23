use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use sqlx::{Postgres, Row};


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
    db_dispatch!(pool, |Db, c| {
        sqlx::query::<Db>(&sql)
            .bind(dir_mimetype_id)
            .bind(dir_mimetype_id)
            .bind(parent_id)
            .bind(storage)
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
            .map(|r| {
                let dirs: i64 = r.get::<Option<i64>, _>("dirs").unwrap_or(0);
                let files: i64 = r.get::<Option<i64>, _>("files").unwrap_or(0);
                (dirs, files)
            })
            .unwrap_or((0, 0))
    })
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
    // Native bigint[] bind on Postgres (PHASE-22 T4).
    match pool {
        DbPool::Pg(p) => {
            let sql = format!(
                "SELECT parent, \
                 count(*) FILTER (WHERE mimetype = $2) AS dirs, \
                 count(*) FILTER (WHERE mimetype != $2) AS files \
                 FROM {prefix}filecache \
                 WHERE parent = ANY($1::bigint[]) AND storage = $3 \
                 GROUP BY parent",
                prefix = prefix,
            );
            sqlx::query::<Postgres>(&sql)
                .bind(parent_ids)
                .bind(dir_mimetype_id)
                .bind(storage)
                .fetch_all(p)
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
        DbPool::Sqlite(p) => {
            // $1 is the directory mimetype id (bound first); the IN list starts at $2.
            let placeholders = (2..=n + 1)
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT parent, \
                 count(*) FILTER (WHERE mimetype = $1) AS dirs, \
                 count(*) FILTER (WHERE mimetype != $1) AS files \
                 FROM {prefix}filecache \
                 WHERE parent IN ({placeholders}) AND storage = ${storage} \
                 GROUP BY parent",
                prefix = prefix,
                storage = n + 2,
            );
            let mut query = sqlx::query(&sql).bind(dir_mimetype_id);
            for id in parent_ids {
                query = query.bind(*id);
            }
            query
                .bind(storage)
                .fetch_all(p)
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
    }
}
