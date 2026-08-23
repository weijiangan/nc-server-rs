use nc_db::pool::DbPool;
use sqlx::{Postgres, Sqlite};
use super::paths::path_hash;
use super::paths::subtree_suffix_offset;


/// Re-key `path`/`path_hash` for every descendant of `old_prefix` onto
/// `new_prefix` (the subtree root itself is left to the caller).
///
/// Postgres does the whole subtree in one statement with DB-side `||` +
/// `md5()`, which is exactly PHP `Cache::moveFromCache`'s child branch
/// (`Cache.php:749-808`); Postgres' `md5(text)` digests the UTF-8 bytes, so
/// it agrees with [`path_hash`]. SQLite has no `md5()` function, so that arm
/// keeps the fetch-and-loop it always had.
pub async fn rekey_subtree_paths(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    old_prefix: &str,
    new_prefix: &str,
) -> Result<(), sqlx::Error> {
    let like = format!("{old_prefix}/%");
    match pool {
        DbPool::Pg(p) => {
            let sql = format!(
                "UPDATE {prefix}filecache \
                 SET path = $1::text || SUBSTRING(path FROM $2::int), \
                     path_hash = md5($1::text || SUBSTRING(path FROM $2::int)) \
                 WHERE storage = $3 AND path LIKE $4"
            );
            sqlx::query::<Postgres>(&sql)
                .bind(new_prefix)
                .bind(subtree_suffix_offset(old_prefix))
                .bind(storage_id)
                .bind(&like)
                .execute(p)
                .await?;
        }
        DbPool::Sqlite(p) => {
            let sql_fetch = format!(
                "SELECT fileid, path FROM {prefix}filecache WHERE storage = $1 AND path LIKE $2"
            );
            let rows: Vec<(i64, String)> = sqlx::query_as::<Sqlite, (i64, String)>(&sql_fetch)
                .bind(storage_id)
                .bind(&like)
                .fetch_all(p)
                .await?;
            let sql_upd =
                format!("UPDATE {prefix}filecache SET path=$1, path_hash=$2 WHERE fileid=$3");
            for (fileid, old_path) in rows {
                let new_path = format!("{new_prefix}{}", &old_path[old_prefix.len()..]);
                sqlx::query::<Sqlite>(&sql_upd)
                    .bind(&new_path)
                    .bind(path_hash(&new_path))
                    .bind(fileid)
                    .execute(p)
                    .await?;
            }
        }
    }
    Ok(())
}
