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
        probe(&client, &class.method, &path, class.depth).await?;
        // Settle: the warmup's fire-and-forget last_activity write (round-4
        // Task 11) lands asynchronously right after the response — wait for
        // it before opening the window so it cannot leak into the measured
        // window.
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        // Measure median-of-3 with ms-precision windows.  Counting filters by
        // the PG log line's OWN millisecond timestamp (not podman's delivery
        // time), so each window is exact: the warmup's statements (written
        // before `start_ms`) and the previous class's stragglers (written
        // before `start_ms`) are excluded.  The median is robust to the
        // residual ±1-2 noise (a fire-and-forget write, a cache TTL edge)
        // while a permanent regression — a statement added to EVERY request —
        // still breaches two of three windows.
        let mut counts = Vec::new();
        for _ in 0..3 {
            let start_ms = now_ms();
            probe(&client, &class.method, &path, class.depth).await?;
            // Let the statements reach the container log before reading.
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            counts.push(count_statements(cfg, start_ms, now_ms())?);
        }
        counts.sort_unstable();
        let statements = counts[counts.len() / 2];
        let _ = &counts;

        match class.name.as_str() {
            "propfind_depth0" => depth0 = Some(statements),
            "propfind_depth1" => depth1 = Some(statements),
            _ => {}
        }
        let pass = statements <= class.budget;
        measurements.push(Measurement {
            class: class.name.clone(),
            statements,
            budget: class.budget,
            pass,
        });
        eprintln!(
            "  {:<18} {:>4} statements (budget {}) {}",
            class.name,
            statements,
            class.budget,
            if pass { "ok" } else { "BREACH" }
        );
        if !pass {
            // Diagnostics: what actually ran in the windows.
            dump_breach(cfg, &class.name, &counts)?;
        }
    }

    // Clean up the PUT probe file (outside any counting window).
    if let Some(path) = created_put {
        let _ = probe(&client, "DELETE", &path, None).await;
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
async fn probe(
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
    let resp = client
        .request(m, path, h, None)
        .await
        .context("probe request")?;
    let status = resp.status();
    // Consume the response body: dav-server-rs streams PROPFIND responses —
    // read_dir and the per-child batch queries run inside the stream, which
    // only executes while the body is being read.  Dropping the response
    // without reading it would skip the very work being measured.
    let body_len = resp
        .text()
        .await
        .context("reading probe response body")?
        .len();
    eprintln!("    [{} {} -> {} ({body_len} B)]", status, method, path);
    Ok(())
}

/// On a breach, dump the statement shapes seen in the class's windows so the
/// extra queries are identifiable without manual log archaeology.
fn dump_breach(cfg: &Config, class: &str, counts: &[u64]) -> Result<()> {
    let out = Command::new("docker")
        .args(["logs", "--since", "60s", &cfg.db_container])
        .output()
        .context("docker logs (breach dump)")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines: Vec<(i64, String)> = Vec::new();
    for line in text.lines().filter(|l| l.contains("execute sqlx")) {
        if let Some(ts) = pg_timestamp_ms(line) {
            lines.push((ts, truncate(line.trim(), 110)));
        }
    }
    eprintln!("  {class} breach — per-window counts: {counts:?}; statement mix in the last 60 s:");
    let mut by_stmt: std::collections::BTreeMap<String, u64> = Default::default();
    for (_, l) in &lines {
        let stmt = l.split("execute ").nth(1).unwrap_or(l).to_string();
        *by_stmt.entry(stmt).or_default() += 1;
    }
    for (stmt, n) in by_stmt.iter().take(20) {
        eprintln!("    {n:>3}  {stmt}");
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Count `execute sqlx` lines whose **PG timestamp** (millisecond precision)
/// falls within `[start_ms, end_ms]`.
///
/// Why the PG timestamp and not podman's: `docker logs --since` filters on
/// the log driver's *ingestion* time, which lags Postgres's write time when
/// the driver batches — late-delivered lines would land in the wrong window
/// (both undercounting the request that produced them and contaminating the
/// next).  Each line carries Postgres's own `YYYY-MM-DD HH:MM:SS.mmm UTC`
/// prefix, so counting by that timestamp makes the window exact.
///
/// The superset read starts 15 s before the window so late-batched lines are
/// still present in the output.  The container log arrives on **stderr**
/// under the podman docker shim, so both streams are merged.
fn count_statements(cfg: &Config, start_ms: i64, end_ms: i64) -> Result<u64> {
    let since = ((start_ms / 1000) as u64).saturating_sub(15);
    let out = Command::new("docker")
        .args(["logs", "--since", &since.to_string(), &cfg.db_container])
        .output()
        .context("docker logs (is the stack up?)")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut n = 0u64;
    for line in text.lines().filter(|l| l.contains("execute sqlx")) {
        if let Some(ts) = pg_timestamp_ms(line) {
            if ts >= start_ms && ts <= end_ms {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Parse the pg log line's own timestamp (`2026-08-10 07:25:05.329 UTC`,
/// fixed-width 23-char prefix) to epoch **milliseconds** — Howard Hinnant's
/// `days_from_civil` without pulling in a date crate.
fn pg_timestamp_ms(line: &str) -> Option<i64> {
    let s = line.get(..23)?;
    let y: i32 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let h: i64 = s.get(11..13)?.parse().ok()?;
    let mi: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    let ms: i64 = s.get(20..23)?.parse().ok()?;
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as i64 * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + mi * 60 + sec) * 1000 + ms)
}
