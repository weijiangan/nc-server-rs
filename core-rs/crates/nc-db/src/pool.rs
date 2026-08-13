use std::collections::HashSet;

use anyhow::Context;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Transaction;

use crate::config::{DbType, NcConfig};

/// The connection pool for the configured database (PHASE-22 T3).
///
/// Replaces `sqlx::AnyPool`: the pool is a native `PgPool` / `SqlitePool`
/// behind a small enum, so every call site binds native arguments and decodes
/// rows without the `Any` driver's per-cell boxing (PHASE-22 T3.3 — the
/// `Executor` delegation and the `any` cargo feature are gone).
///
/// All call sites query the enum per-variant; the enum implements sqlx's
/// `Executor` by delegating to the inner native pool (T3.1), and the
/// dialect checks key on the variant itself.  The old translation of
/// The `any` driver, its feature, and the delegation machinery were removed
/// in T3.3 once every call site was migrated to per-variant native queries.
#[derive(Clone, Debug)]
pub enum DbPool {
    Pg(PgPool),
    Sqlite(SqlitePool),
}

impl DbPool {
    /// Is the pool backed by PostgreSQL?  The dialect is fixed for a running
    /// server (CLAUDE.md principle 6) — this replaces the old process-global
    /// `backend_is_postgres()` latch with the enum variant itself.
    pub fn is_postgres(&self) -> bool {
        matches!(self, DbPool::Pg(_))
    }

    /// Begin a transaction on the pool.
    ///
    /// The transaction owns its pooled connection (`Transaction<'static,
    /// _>`); `&DbPool` cannot implement sqlx's `Acquire` (that would require
    /// fabricating an `AnyConnection`), so `begin()` is an inherent method
    /// and the transaction is the `DbTxn` enum (PHASE-22 T3.2).
    pub async fn begin(&self) -> anyhow::Result<DbTxn> {
        match self {
            DbPool::Pg(pool) => {
                let tx: Transaction<'static, sqlx::Postgres> = pool.begin().await?;
                Ok(DbTxn::Pg(tx))
            }
            DbPool::Sqlite(pool) => {
                let tx: Transaction<'static, sqlx::Sqlite> = pool.begin().await?;
                Ok(DbTxn::Sqlite(tx))
            }
        }
    }
}

/// An in-flight transaction owned by [`DbPool::begin`].
///
/// Call sites match on the variant for the native transaction (T3.3);
/// `commit` / `rollback` / the dialect check are inherent methods.
pub enum DbTxn {
    Pg(Transaction<'static, sqlx::Postgres>),
    Sqlite(Transaction<'static, sqlx::Sqlite>),
}

impl std::fmt::Debug for DbTxn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbTxn::Pg(_) => f.write_str("DbTxn::Pg"),
            DbTxn::Sqlite(_) => f.write_str("DbTxn::Sqlite"),
        }
    }
}

impl DbTxn {
    pub fn is_postgres(&self) -> bool {
        matches!(self, DbTxn::Pg(_))
    }

    pub async fn commit(self) -> Result<(), sqlx::Error> {
        match self {
            DbTxn::Pg(tx) => tx.commit().await,
            DbTxn::Sqlite(tx) => tx.commit().await,
        }
    }

    pub async fn rollback(self) {
        match self {
            DbTxn::Pg(tx) => {
                let _ = tx.rollback().await;
            }
            DbTxn::Sqlite(tx) => {
                let _ = tx.rollback().await;
            }
        }
    }
}


// ─── pool construction ───────────────────────────────────────────────────────

/// Count physical cores, excluding hyperthreads.
///
/// On Linux, physical cores are the unique `(physical_package_id, core_id)`
/// pairs in sysfs — hyperthreads share a `core_id` and must not inflate the
/// pool size (the production server reports 2 physical cores where
/// `nproc`/`available_parallelism` would say 4 logical).  Falls back to
/// logical CPUs where sysfs is unavailable.
fn physical_cores() -> usize {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut i = 0usize;
    loop {
        let core =
            std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{i}/topology/core_id"));
        let pkg = std::fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{i}/topology/physical_package_id"
        ));
        match (core, pkg) {
            (Ok(c), Ok(p)) => {
                seen.insert((p.trim().to_string(), c.trim().to_string()));
                i += 1;
            }
            _ => break,
        }
    }
    if !seen.is_empty() {
        seen.len()
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
}

/// Build a connection pool for the database described in `config`.
///
/// The pool is created but not yet used — the caller drives the
/// `sqlx::migrate!()` step before opening the HTTP listener.  The pool is a
/// native `PgPool` / `SqlitePool` behind the `DbPool` enum (PHASE-22 T3);
/// no `Any` driver registry is involved.
pub async fn build_pool(config: &NcConfig) -> anyhow::Result<DbPool> {
    let min = 5u32;
    // 4× physical cores (hyperthreads excluded), floored at 16 so the
    // 6-query depth-1 PROPFIND batch (`read_dir` join, phase-21 S1 / T6)
    // plus concurrent traffic never queues on the pool, capped at 64 so big
    // hosts don't thrash Postgres backends.  A Rust server actually
    // saturates its DB — 50 fixed backends was arbitrary.
    // 2-core prod → 16; 6-core dev → 24; 16-core → 64.
    let cores = physical_cores() as u32;
    let max = (cores * 4).clamp(16, 64);

    // No ping on acquire: with ~9 sequential fetch_*(pool) calls per
    // PROPFIND that is ~9 pure-overhead RTTs (sqlx pings every idle
    // acquire by default; the Postgres ping is a full flush round trip).
    // Dead connections are detected on first use and discarded;
    // max_lifetime/idle_timeout prune idle ones.
    let pool = match config.dbtype {
        DbType::Pgsql => {
            let host = config.dbhost.as_deref().unwrap_or("localhost");
            let name = config
                .dbname
                .as_deref()
                .context("dbname is required for pgsql")?;
            let user = config
                .dbuser
                .as_deref()
                .context("dbuser is required for pgsql")?;
            let pass = config.dbpassword.as_deref().unwrap_or("");
            let url = format!("postgresql://{user}:{pass}@{host}/{name}");
            let pool = PgPoolOptions::new()
                .min_connections(min)
                .max_connections(max)
                .test_before_acquire(false)
                .connect(&url)
                .await
                .with_context(|| format!("Failed to connect to database at {}", redact(&url)))?;
            // Verify connectivity.
            sqlx::query("SELECT 1")
                .execute(&pool)
                .await
                .context("Database health-check query failed")?;
            DbPool::Pg(pool)
        }
        DbType::Sqlite => {
            // For SQLite the "host" is a file path in dbname.
            let path = config.dbname.as_deref().unwrap_or("nextcloud.db");
            let url = format!("sqlite://{path}?mode=rwc");
            let pool = SqlitePoolOptions::new()
                .min_connections(min)
                .max_connections(max)
                .test_before_acquire(false)
                .connect(&url)
                .await
                .with_context(|| format!("Failed to connect to database at {}", redact(&url)))?;
            // Verify connectivity.
            sqlx::query("SELECT 1")
                .execute(&pool)
                .await
                .context("Database health-check query failed")?;
            DbPool::Sqlite(pool)
        }
    };

    tracing::info!(
        driver = %match config.dbtype {
            DbType::Pgsql => "postgres",
            DbType::Sqlite => "sqlite",
        },
        "Database pool ready (min={min}, max={max})"
    );

    Ok(pool)
}

/// Redact password from a connection URL for safe logging.
fn redact(url: &str) -> String {
    if let Some(at) = url.rfind('@') {
        if let Some(scheme_end) = url.find("://") {
            let scheme = &url[..scheme_end + 3];
            let after_at = &url[at..];
            return format!("{scheme}***{after_at}");
        }
    }
    url.to_string()
}
