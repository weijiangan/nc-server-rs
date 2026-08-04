//! Fail-fast parity checks run before any scenario: both instances must be up,
//! on the same **numeric** version, and with the same enabled-app set. Any
//! drift makes the differential meaningless (it would report config divergence
//! as if it were a Rust bug).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sqlx::postgres::PgPool;

use crate::client::NextcloudClient;
use crate::config::{Config, Instance};

#[derive(Debug, Deserialize)]
struct Status {
    installed: bool,
    version: String,
}

async fn fetch_status(inst: &Instance, user: &str, pass: &str) -> Result<Status> {
    let client = NextcloudClient::new(inst, user, pass)?;
    let resp = client.get("/status.php").await?;
    if !resp.status().is_success() {
        bail!("status.php returned {}", resp.status());
    }
    let body = resp.text().await.context("reading status.php body")?;
    serde_json::from_str(&body)
        .with_context(|| format!("parsing status.php body ({body:?})"))
}

/// Enabled apps on one instance. Column is **`configvalue`** (live-verified
/// `oc_appconfig` = appid, configkey, configvalue, type, lazy) — not `value`.
async fn enabled_apps(dsn: &str) -> Result<Vec<String>> {
    let pool = PgPool::connect(dsn)
        .await
        .with_context(|| format!("connecting to {dsn}"))?;
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT appid FROM oc_appconfig \
         WHERE configkey = 'enabled' AND configvalue = 'yes' ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .context("querying enabled apps")?;
    Ok(rows)
}

/// Run all preconditions; return `Ok` only if the two instances are comparable.
pub async fn check(cfg: &Config) -> Result<()> {
    let sut = fetch_status(&cfg.sut, &cfg.admin_user, &cfg.admin_pass)
        .await
        .with_context(|| format!("SUT status ({})", cfg.sut.base_url))?;
    let oracle = fetch_status(&cfg.oracle, &cfg.admin_user, &cfg.admin_pass)
        .await
        .with_context(|| format!("oracle status ({})", cfg.oracle.base_url))?;

    if !sut.installed {
        bail!("SUT is not installed");
    }
    if !oracle.installed {
        bail!("oracle is not installed");
    }

    // Numeric-version parity. The `versionstring` may differ by a " dev" suffix
    // (an install-time artifact) and must NOT be compared.
    if sut.version != oracle.version {
        bail!(
            "version mismatch: SUT={} oracle={} (compare numeric `version`, not `versionstring`)",
            sut.version,
            oracle.version
        );
    }

    let sut_apps = enabled_apps(&cfg.sut.dsn)
        .await
        .with_context(|| format!("SUT enabled apps ({})", cfg.sut.dsn))?;
    let oracle_apps = enabled_apps(&cfg.oracle.dsn)
        .await
        .with_context(|| format!("oracle enabled apps ({})", cfg.oracle.dsn))?;
    if sut_apps != oracle_apps {
        let sut_only = minus(&sut_apps, &oracle_apps);
        let oracle_only = minus(&oracle_apps, &sut_apps);
        bail!(
            "enabled-app set differs:\n  SUT-only:    {sut_only:?}\n  oracle-only: {oracle_only:?}"
        );
    }

    Ok(())
}

fn minus<'a>(a: &'a [String], b: &[String]) -> Vec<&'a str> {
    a.iter().filter(|x| !b.contains(x)).map(|s| s.as_str()).collect()
}
