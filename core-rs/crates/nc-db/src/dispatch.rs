//! Backend dispatch for the two supported dialects.
//!
//! Every query has to be issued against a native `PgPool` or `SqlitePool` —
//! [`DbPool`](crate::pool::DbPool) is an enum, not an executor, so that binds
//! and row decoding stay natively monomorphized (PHASE-22 T3.3 removed the
//! `Any` driver precisely to avoid its per-cell boxing).  Spelling the
//! `match` out at every call site meant writing each statement twice, which
//! is pure duplication and lets the two arms drift apart unnoticed.
//!
//! [`db_dispatch!`] writes the arms once and expands them per variant, so the
//! generated code is exactly the hand-written `match` it replaces — same
//! native types, same binds, no dynamic dispatch.  Genuinely dialect-specific
//! SQL (Postgres `ANY($1)`, `FILTER`, recursive CTEs) still needs a real
//! `match`; these macros are only for statements that are identical on both.

/// Run one query body against whichever backend the pool holds.
///
/// The body is expanded once per variant with `$db` bound to that variant's
/// sqlx `Database` type and `$conn` to its native pool.
///
/// ```ignore
/// let rows = db_dispatch!(&self.pool, |Db, c| {
///     sqlx::query::<Db>(&sql).bind(id).fetch_all(c).await
/// });
/// ```
#[macro_export]
macro_rules! db_dispatch {
    ($pool:expr, |$db:ident, $conn:ident| $body:expr) => {
        match $pool {
            $crate::pool::DbPool::Pg($conn) => {
                use ::sqlx::Postgres as $db;
                $body
            }
            $crate::pool::DbPool::Sqlite($conn) => {
                use ::sqlx::Sqlite as $db;
                $body
            }
        }
    };
}

/// `INSERT`/`UPDATE`/`DELETE`, discarding the row count.
///
/// Yields `Result<(), sqlx::Error>` — callers log or propagate it; never
/// swallow it (CLAUDE.md engineering hygiene #1).
#[macro_export]
macro_rules! db_execute {
    ($pool:expr, $sql:expr $(, $bind:expr)* $(,)?) => {
        $crate::db_dispatch!($pool, |Db, c| {
            ::sqlx::query::<Db>($sql)$(.bind($bind))*
                .execute(c)
                .await
                .map(|_| ())
        })
    };
}

/// A single scalar column; `Ok(None)` when no row matched.
#[macro_export]
macro_rules! db_scalar_opt {
    ($pool:expr, $sql:expr $(, $bind:expr)* $(,)?) => {
        $crate::db_dispatch!($pool, |Db, c| {
            ::sqlx::query_scalar::<Db, _>($sql)$(.bind($bind))*
                .fetch_optional(c)
                .await
        })
    };
}

/// A single scalar column from exactly one row.
#[macro_export]
macro_rules! db_scalar_one {
    ($pool:expr, $sql:expr $(, $bind:expr)* $(,)?) => {
        $crate::db_dispatch!($pool, |Db, c| {
            ::sqlx::query_scalar::<Db, _>($sql)$(.bind($bind))*
                .fetch_one(c)
                .await
        })
    };
}

/// One scalar column across every matching row.
#[macro_export]
macro_rules! db_scalar_all {
    ($pool:expr, $sql:expr $(, $bind:expr)* $(,)?) => {
        $crate::db_dispatch!($pool, |Db, c| {
            ::sqlx::query_scalar::<Db, _>($sql)$(.bind($bind))*
                .fetch_all(c)
                .await
        })
    };
}

/// Every matching row, each mapped to a backend-independent value.
///
/// The per-row closure is required: `PgRow` and `SqliteRow` are distinct
/// types, so the mapping has to happen inside each arm for them to unify.
#[macro_export]
macro_rules! db_rows {
    ($pool:expr, $sql:expr, [$($bind:expr),* $(,)?], |$row:ident| $map:expr) => {
        $crate::db_dispatch!($pool, |Db, c| {
            ::sqlx::query::<Db>($sql)$(.bind($bind))*
                .fetch_all(c)
                .await
                .map(|rows| rows.into_iter().map(|$row| $map).collect::<Vec<_>>())
        })
    };
}

/// The first matching row mapped to a backend-independent value, if any.
#[macro_export]
macro_rules! db_row_opt {
    ($pool:expr, $sql:expr, [$($bind:expr),* $(,)?], |$row:ident| $map:expr) => {
        $crate::db_dispatch!($pool, |Db, c| {
            ::sqlx::query::<Db>($sql)$(.bind($bind))*
                .fetch_optional(c)
                .await
                .map(|r| r.map(|$row| $map))
        })
    };
}
