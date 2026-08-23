use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use sqlx::{Postgres, Row};


// ─── Phase 12.6: comments properties ───────────────────────────────────────────

/// Return the number of top-level comments for a file, matching PHP
/// `ICommentsManager::getNumberOfCommentsForObject('files', $id)`.
pub async fn get_comments_count(pool: &DbPool, prefix: &str, fileid: i64) -> i64 {
    let sql = format!(
        "SELECT COUNT(*) FROM {prefix}comments \
         WHERE object_type = 'files' AND object_id = $1",
        prefix = prefix
    );
    db_dispatch!(pool, |Db, c| {
        sqlx::query_scalar::<Db, Option<i64>>(&sql)
            .bind(fileid.to_string())
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
            .flatten()
            .unwrap_or(0)
    })
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
    db_dispatch!(pool, |Db, c| {
        sqlx::query_scalar::<Db, Option<i64>>(&sql)
            .bind(fileid.to_string())
            .bind(uid)
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
            .flatten()
            .unwrap_or(0)
    })
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
    // Native text[] bind on Postgres (PHASE-22 T4): object_id is a text
    // column, so the ids bind as strings.
    match pool {
        DbPool::Pg(p) => {
            let sql = format!(
                "SELECT c.object_id, COUNT(*) AS n, \
                 count(*) FILTER (WHERE c.actor_type = 'users' AND c.actor_id != $2 \
                     AND c.creation_timestamp > COALESCE(m.marker_datetime, '1970-01-01 00:00:00')) AS unread \
                 FROM {prefix}comments c \
                 LEFT JOIN {prefix}comments_read_markers m \
                   ON m.user_id = $2 AND m.object_type = 'files' AND m.object_id = c.object_id \
                 WHERE c.object_type = 'files' \
                 AND c.object_id = ANY($1::text[]) \
                 GROUP BY c.object_id",
                prefix = prefix,
            );
            let ids: Vec<String> = fileids.iter().map(i64::to_string).collect();
            sqlx::query::<Postgres>(&sql)
                .bind(&ids)
                .bind(uid)
                .fetch_all(p)
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
        DbPool::Sqlite(p) => {
            let placeholders = (1..=n)
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
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
            );
            let mut query = sqlx::query(&sql);
            for id in fileids {
                query = query.bind(id.to_string());
            }
            query
                .bind(uid)
                .fetch_all(p)
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
    }
}


/// Build the `{oc:}comments-href` URL, matching PHP
/// `CommentPropertiesPlugin::getCommentsLink()`.
///
/// The format is: `{base_url}/remote.php/dav/comments/files/{fileid}`
pub fn build_comments_href(base_url: &str, fileid: i64) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{base}/remote.php/dav/comments/files/{fileid}")
}
