use std::collections::HashSet;
use std::sync::OnceLock;

use anyhow::Context;
use sqlx::any::AnyPoolOptions;

pub use sqlx::AnyPool as DbPool;

use crate::config::{DbType, NcConfig};

/// Cached backend dialect (CLAUDE.md principle 6 — cache the backend once).
/// Set by `build_pool` from `config.dbtype` before the HTTP listener opens;
/// tests that build their own pools without `build_pool` see `false` and
/// take the SQLite `IN (...)` path.
static BACKEND_IS_POSTGRES: OnceLock<bool> = OnceLock::new();

/// Is the process's pool backed by PostgreSQL?  Consumed by the dialect
/// branches in `nc-dav` (e.g. `= ANY(string_to_array($1, ','))` on Postgres
/// vs `IN ($1, …)` on SQLite — the Any driver cannot bind arrays).
pub fn backend_is_postgres() -> bool {
    *BACKEND_IS_POSTGRES.get().unwrap_or(&false)
}

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
        let core = std::fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{i}/topology/core_id"
        ));
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
/// `sqlx::migrate!()` step before opening the HTTP listener.
pub async fn build_pool(config: &NcConfig) -> anyhow::Result<DbPool> {
    // Register all compiled-in AnyPool drivers (sqlite, postgres).
    // Must be called before any AnyPool connection attempt.
    sqlx::any::install_default_drivers();

    let url = connection_url(config)?;

    let _ = BACKEND_IS_POSTGRES.set(config.dbtype == DbType::Pgsql);

    let min = 5u32;
    // 4× physical cores (hyperthreads excluded), floored at 16 so the
    // 8-query depth-1 PROPFIND batch (`read_dir` join, phase-21 S1) plus
    // concurrent traffic never queues on the pool, capped at 64 so big
    // hosts don't thrash Postgres backends.  A Rust server actually
    // saturates its DB — 50 fixed backends was arbitrary.
    // 2-core prod → 16; 6-core dev → 24; 16-core → 64.
    let cores = physical_cores() as u32;
    let max = (cores * 4).clamp(16, 64);

    let pool = AnyPoolOptions::new()
        .min_connections(min)
        .max_connections(max)
        // No ping on acquire: with ~9 sequential fetch_*(pool) calls per
        // PROPFIND that is ~9 pure-overhead RTTs (sqlx pings every idle
        // acquire by default; the Postgres ping is a full flush round trip).
        // Dead connections are detected on first use and discarded;
        // max_lifetime/idle_timeout prune idle ones.
        .test_before_acquire(false)
        .connect(&url)
        .await
        .with_context(|| format!("Failed to connect to database at {}", redact(&url)))?;
    // Verify connectivity.
    sqlx::query("SELECT 1")
        .execute(&pool)
        .await
        .context("Database health-check query failed")?;

    tracing::info!(
        driver = %url.split(':').next().unwrap_or("?"),
        "Database pool ready (min={min}, max={max})"
    );

    Ok(pool)
}

fn connection_url(config: &NcConfig) -> anyhow::Result<String> {
    match config.dbtype {
        DbType::Sqlite => {
            // For SQLite the "host" is a file path in dbname.
            let path = config.dbname.as_deref().unwrap_or("nextcloud.db");
            Ok(format!("sqlite://{path}?mode=rwc"))
        }
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
            Ok(format!("postgresql://{user}:{pass}@{host}/{name}"))
        }
    }
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
