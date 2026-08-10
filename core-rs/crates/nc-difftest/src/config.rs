//! Harness configuration: base URLs, `Host` headers, PostgreSQL DSNs, container
//! names and credentials for the SUT (Rust) and the Oracle (pure PHP).
//!
//! Every field is overridable via `NC_DIFFTEST_*` environment variables so the
//! harness can point at any stack; the defaults match the local dev docker set
//! up by `make diff-up` (Phase 16.1).

use anyhow::Result;

/// One Nextcloud instance under comparison.
#[derive(Debug, Clone)]
pub struct Instance {
    /// Base URL, e.g. `http://127.0.0.1:8080`.
    pub base_url: String,
    /// `Host` header to send. Both instances gate on trusted domains derived
    /// from their `VIRTUAL_HOST` (`nextcloud.local` / `oracle.local`).
    pub host: String,
    /// PostgreSQL DSN, e.g. `postgres://postgres:postgres@127.0.0.1:8212/nextcloud`.
    pub dsn: String,
    /// Container name for `docker exec` file-tree snapshots (Phase 16.8).
    pub container: String,
}

/// Full harness configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub sut: Instance,
    pub oracle: Instance,
    pub admin_user: String,
    pub admin_pass: String,
    /// Data directory inside the containers (Phase 16.8 file-tree snapshots).
    pub data_dir: String,
    /// Postgres container name for the phase-20 budget gate's statement log
    /// counting (`docker logs`).  The SUT and oracle share one database
    /// container on the dev stack.
    pub db_container: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    /// Load from `NC_DIFFTEST_*` env vars with dev-docker defaults.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            sut: Instance {
                base_url: env_or("NC_DIFFTEST_SUT_URL", "http://127.0.0.1:8080"),
                host: env_or("NC_DIFFTEST_SUT_HOST", "nextcloud.local"),
                dsn: env_or(
                    "NC_DIFFTEST_SUT_DSN",
                    "postgres://postgres:postgres@127.0.0.1:8212/nextcloud",
                ),
                container: env_or("NC_DIFFTEST_SUT_CONTAINER", "master-nextcloud-1"),
            },
            oracle: Instance {
                base_url: env_or("NC_DIFFTEST_ORACLE_URL", "http://127.0.0.1:9091"),
                host: env_or("NC_DIFFTEST_ORACLE_HOST", "oracle.local"),
                dsn: env_or(
                    "NC_DIFFTEST_ORACLE_DSN",
                    "postgres://postgres:postgres@127.0.0.1:8212/oracle",
                ),
                container: env_or("NC_DIFFTEST_ORACLE_CONTAINER", "master-oracle-1"),
            },
            admin_user: env_or("NC_DIFFTEST_ADMIN_USER", "admin"),
            admin_pass: env_or("NC_DIFFTEST_ADMIN_PASS", "admin"),
            data_dir: env_or("NC_DIFFTEST_DATADIR", "/var/www/html/data"),
            db_container: env_or("NC_DIFFTEST_DB_CONTAINER", "master-database-pgsql-1"),
        })
    }
}
