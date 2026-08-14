//! Preview-generation queue (`oc_preview_generation`) on write.
//!
//! PHP's `previewgenerator` app registers a `PostWriteListener` on
//! `NodeWrittenEvent` (`apps-extra/previewgenerator/lib/Listeners/PostWriteListener.php`)
//! that, for every successful file write, inserts a `(uid, file_id, queued_at)` row
//! into `oc_preview_generation` **if no row for that `(uid, file_id)` exists yet** —
//! a background job later pre-generates the previews and drains the queue.
//!
//! The native Rust PUT fires no PHP event, so it must reproduce this side effect
//! itself or the differential oracle reports the missing queue row (finding #5 /
//! phase-16.4). The table has no unique constraint on `(uid, file_id)` (the app
//! migrations add only the `id` PK, later `queued_at`), so we guard with an
//! existence check rather than `ON CONFLICT`, mirroring the PHP SELECT-then-INSERT.

use nc_db::pool::DbPool;
use tracing::warn;

/// Queue preview generation for `file_id` owned by `uid`, unless already queued.
///
/// `queued_at` is the current unix time. Errors are logged, never fatal to the PUT
/// (the queue only affects background pre-generation, not the write itself).
pub(crate) async fn queue_preview_generation(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    file_id: i64,
    queued_at: i64,
) {
    let sql = format!(
        "INSERT INTO {prefix}preview_generation (uid, file_id, queued_at) \
         SELECT $1, $2, $3 \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM {prefix}preview_generation \
             WHERE uid = $1 AND file_id = $2 \
         )",
        prefix = prefix
    );
    // The columns are `integer` (INT4) on Postgres — bind at their exact SQL
    // types or the strict native driver rejects the statement (every file
    // write then fails to queue, starving background preview generation).
    let result = match pool {
        DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&sql)
            .bind(uid)
            .bind(file_id as i32)
            .bind(queued_at as i32)
            .execute(p)
            .await
            .map(|_| ()),
        DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&sql)
            .bind(uid)
            .bind(file_id)
            .bind(queued_at)
            .execute(p)
            .await
            .map(|_| ()),
    };
    if let Err(e) = result {
        warn!(uid, file_id, error = %e, "Failed to queue preview generation");
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory SQLite with the `oc_preview_generation` shape the app migrations
    /// produce (`id` PK autoincrement, `uid`, `file_id`, `queued_at`).
    async fn fresh_db() -> DbPool {
        let pool = DbPool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("in-memory SQLite"),
        );
        match &pool {
            DbPool::Pg(p) => {
                sqlx::query::<sqlx::Postgres>(
                    "CREATE TABLE oc_preview_generation (
                        id        INTEGER NOT NULL PRIMARY KEY,
                        uid       VARCHAR(256) NOT NULL,
                        file_id   BIGINT NOT NULL,
                        queued_at BIGINT NOT NULL
                    )",
                )
                .execute(p)
                .await
                .expect("create table");
            }
            DbPool::Sqlite(p) => {
                sqlx::query::<sqlx::Sqlite>(
                    "CREATE TABLE oc_preview_generation (
                        id        INTEGER NOT NULL PRIMARY KEY,
                        uid       VARCHAR(256) NOT NULL,
                        file_id   BIGINT NOT NULL,
                        queued_at BIGINT NOT NULL
                    )",
                )
                .execute(p)
                .await
                .expect("create table");
            }
        }
        pool
    }

    async fn count(pool: &DbPool) -> i64 {
        match pool {
            DbPool::Pg(p) => sqlx::query_scalar::<sqlx::Postgres, _>(
                "SELECT COUNT(*) FROM oc_preview_generation",
            )
            .fetch_one(p)
            .await
            .expect("count"),
            DbPool::Sqlite(p) => sqlx::query_scalar::<sqlx::Sqlite, _>(
                "SELECT COUNT(*) FROM oc_preview_generation",
            )
            .fetch_one(p)
            .await
            .expect("count"),
        }
    }

    #[tokio::test]
    async fn queue_inserts_once_per_uid_file() {
        let pool = fresh_db().await;
        queue_preview_generation(&pool, "oc_", "admin", 42, 1_700_000_000).await;
        assert_eq!(count(&pool).await, 1);
        // Second call for the same (uid, file_id) is a no-op.
        queue_preview_generation(&pool, "oc_", "admin", 42, 1_700_000_100).await;
        assert_eq!(count(&pool).await, 1);
        // A different file_id queues a new row.
        queue_preview_generation(&pool, "oc_", "admin", 43, 1_700_000_200).await;
        assert_eq!(count(&pool).await, 2);
    }
}
