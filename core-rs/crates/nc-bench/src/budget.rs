//! `nc-bench budget` — the Phase 20 query-count budget gate.
//!
//! Measures per-request-class statement counts on the SUT and fails when any
//! class exceeds its budget (the "bundle-size budget" for queries).  Like the
//! rest of the harness it is black-box: it speaks HTTP through the same
//! `NextcloudClient` and links no `nc-*` server crate.
//!
//! Counting mechanism: enable Postgres `log_statement='all'` on the SUT's
//! database, run each probe once inside a tight window, and count
//! `execute sqlx` lines in the database container's log since the window
//! opened.  The `execute sqlx` filter matches only Rust's prepared statements
//! (`sqlx_s_N`); PHP/Doctrine logs as `<unnamed>` and background cron is
//! excluded by construction.  Budgets come from `perf-budget.yaml`.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;

use nc_difftest::client::NextcloudClient;
use nc_difftest::config::Config;

use crate::auth;

#[derive(Debug, Deserialize)]
pub struct BudgetFile {
    pub classes: Vec<BudgetClass>,
    #[serde(rename = "scaling_delta_budget")]
    pub scaling_delta_budget: u64,
}

#[derive(Debug, Deserialize)]
pub struct BudgetClass {
    pub name: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub unique_path: bool,
    pub budget: u64,
}

#[derive(Debug)]
struct Measurement {
    class: String,
    statements: u64,
    budget: u64,
    pass: bool,
}

/// Run the budget gate against the live stack.
pub async fn run(cfg: &Config, budget_path: &str) -> Result<bool> {
    let budget_file: BudgetFile = serde_yaml::from_str(
        &std::fs::read_to_string(budget_path)
            .with_context(|| format!("reading budget file {budget_path}"))?,
    )
    .with_context(|| format!("parsing budget file {budget_path}"))?;

    // App-token auth (the hot path) — plain password would add the argon2
    // KDF and the oc_users lookup to every measurement.
    let raw_token = auth::create_token(&cfg.sut, &cfg.admin_user).await?;
    let client = NextcloudClient::new(&cfg.sut, &cfg.admin_user, &raw_token)?;

    // Enable statement logging on the SUT's Postgres (superuser DSN).
    let pool = sqlx::PgPool::connect(&cfg.sut.dsn)
        .await
        .context("connecting to the SUT Postgres for statement logging")?;
    sqlx::query("ALTER SYSTEM SET log_statement = 'all'")
        .execute(&pool)
        .await
        .context("enabling log_statement")?;
    sqlx::query("SELECT pg_reload_conf()")
        .execute(&pool)
        .await
        .context("reloading Postgres config")?;
    // The gate's own ALTER/reload ran before any window opens, so they are
    // never counted.

    let mut measurements: Vec<Measurement> = Vec::new();
    let mut depth0: Option<u64> = None;
    let mut depth1: Option<u64> = None;
    let mut created_put: Option<String> = None;

    for class in &budget_file.classes {
        let path = if class.unique_path {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let p = class.path.replace("<ts>", &ts.to_string());
            created_put = Some(p.clone());
            p
        } else {
            class.path.clone()
        };

        // Unmeasured warmup (caches + throttler state reach steady state).
        request(&client, &class.method, &path, class.depth).await?;

        // Open the window and measure one request.
        let window = open_window(cfg)?;
        request(&client, &class.method, &path, class.depth).await?;
        let statements = count_statements(cfg, window)?;

        match class.name.as_str() {
            "propfind_depth0" => depth0 = Some(statements),
            "propfind_depth1" => depth1 = Some(statements),
            _ => {}
        }
        measurements.push(Measurement {
            class: class.name.clone(),
            statements,
            budget: class.budget,
            pass: statements <= class.budget,
        });
        eprintln!(
            "  {:<18} {:>4} statements (budget {}) {}",
            class.name,
            statements,
            class.budget,
            if statements <= class.budget { "ok" } else { "BREACH" }
        );
    }

    // Clean up the PUT probe file (outside any counting window).
    if let Some(path) = created_put {
        let _ = request(&client, "DELETE", &path, None).await;
    }

    // Disable statement logging.
    let _ = sqlx::query("ALTER SYSTEM SET log_statement = 'none'")
        .execute(&pool)
        .await;
    let _ = sqlx::query("SELECT pg_reload_conf()").execute(&pool).await;

    // Scaling-delta check: depth1's extra cost over depth0 must stay at the
    // fixed batch cost (~8).  Any per-child query reintroduction breaches it.
    let mut all_pass = measurements.iter().all(|m| m.pass);
    if let (Some(d0), Some(d1)) = (depth0, depth1) {
        let delta = d1.saturating_sub(d0);
        let delta_pass = delta <= budget_file.scaling_delta_budget;
        all_pass &= delta_pass;
        eprintln!(
            "  scaling delta: depth1({d1}) - depth0({d0}) = {delta} (budget {}) {}",
            budget_file.scaling_delta_budget,
            if delta_pass { "ok" } else { "BREACH" }
        );
    } else {
        eprintln!("  (scaling delta skipped: depth0/depth1 classes not in the budget file)");
    }

    // Machine-readable summary on stdout (progress went to stderr).
    println!("class,statements,budget,pass");
    for m in &measurements {
        println!("{},{},{},{}", m.class, m.statements, m.budget, m.pass);
    }
    Ok(all_pass)
}

/// One probe request with the configured method + optional Depth header.
async fn request(
    client: &NextcloudClient,
    method: &str,
    path: &str,
    depth: Option<u32>,
) -> Result<()> {
    let m = reqwest::Method::from_bytes(method.as_bytes())
        .with_context(|| format!("unknown probe method {method}"))?;
    let mut h = reqwest::header::HeaderMap::new();
    if let Some(d) = depth {
        h.insert("Depth", reqwest::header::HeaderValue::from(d));
    }
    let resp = client.request(m, path, h, None).await.context("probe request")?;
    eprintln!("    [{} {} -> {}]", resp.status(), method, path);
    // Let the statements flush to the container log before counting (podman's
    // log driver batches stdout — the manual measurement needed ~1 s).
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    Ok(())
}

/// Window start as a unix timestamp (docker `--since` accepts unix seconds).
fn open_window(_cfg: &Config) -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs())
}

/// Count `execute sqlx` lines in the DB container's log since `since_secs`.
///
/// Plain unix seconds (no `@` prefix — podman's docker shim rejects `@`).
/// The container log arrives on **stderr** under the podman docker shim, so
/// both streams are merged (stderr lines are only counted if they match the
/// `execute sqlx` pattern; the shim's own "Emulate Docker CLI" notice does
/// not).
fn count_statements(cfg: &Config, since_secs: u64) -> Result<u64> {
    let out = Command::new("docker")
        .args(["logs", "--since", &since_secs.to_string(), &cfg.db_container])
        .output()
        .context("docker logs (is the stack up?)")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(text.lines().filter(|l| l.contains("execute sqlx")).count() as u64)
}
