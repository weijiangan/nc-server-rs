use anyhow::Context;
use sqlx::any::AnyPoolOptions;

pub use sqlx::AnyPool as DbPool;

use crate::config::{DbType, NcConfig};

/// Build a connection pool for the database described in `config`.
///
/// The pool is created but not yet used — the caller drives the
/// `sqlx::migrate!()` step before opening the HTTP listener.
pub async fn build_pool(config: &NcConfig) -> anyhow::Result<DbPool> {
    // Register all compiled-in AnyPool drivers (sqlite, postgres).
    // Must be called before any AnyPool connection attempt.
    sqlx::any::install_default_drivers();

    let url = connection_url(config)?;

    let min = 5u32;
    let max = 50u32;

    let pool = AnyPoolOptions::new()
        .min_connections(min)
        .max_connections(max)
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
