//! `bench scenario` — replay difftest scenarios, compare per-op latencies.
//!
//! Reuses nc-difftest's `scenario::run` / `run_cleanup` and `OpResult.elapsed`
//! (Phase 17 addition) so the benchmark measures exactly the same operation
//! sequences the differential suite replays.  Warmup pass first (PHP opcache +
//! Rust caches), then measured iterations alternating the starting side so
//! drift cancels instead of biasing one side.  Cleanup ops run unmeasured
//! after each iteration so the scenario stays re-runnable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use nc_difftest::client::NextcloudClient;
use nc_difftest::config::Config;
use nc_difftest::scenario::{self, OpResult, Scenario};

use crate::auth;
use crate::report::{self, OpStat, ScenarioReport, Stats};

/// Directory holding the difftest scenario YAMLs.  Overridable for installed
/// runs of the binary.
fn scenarios_dir() -> PathBuf {
    match std::env::var("NC_BENCH_SCENARIOS_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nc-difftest/scenarios"),
    }
}

/// Run the scenario corpus (or the `sel` subset by file stem) and return the
/// per-op latency comparison.  Progress goes to stderr so `--json` stdout
/// stays machine-clean.
pub async fn bench(
    cfg: &Config,
    sel: &[String],
    iterations: u32,
    warmup: u32,
) -> Result<Vec<ScenarioReport>> {
    nc_difftest::preconditions::check(cfg)
        .await
        .context("preconditions failed — is the stack up (`make diff-up`) and the sides identical?")?;

    // Clear throttler state, then authenticate with per-instance app tokens
    // (hot path — see `auth`).
    auth::reset_throttle(&cfg.sut).await;
    auth::reset_throttle(&cfg.oracle).await;
    let (sut_user, sut_pass) =
        auth::instance_creds(&cfg.sut, &cfg.admin_user, &cfg.admin_pass).await?;
    let (oracle_user, oracle_pass) =
        auth::instance_creds(&cfg.oracle, &cfg.admin_user, &cfg.admin_pass).await?;

    // One client per side for the WHOLE run: the proxy's upstream keepalive
    // pool degrades when the bench churns connections (a fresh pool per
    // scenario made every request pay a ~200 ms stale-connection stall).
    let sut = NextcloudClient::new(&cfg.sut, &sut_user, &sut_pass)?;
    let oracle = NextcloudClient::new(&cfg.oracle, &oracle_user, &oracle_pass)?;

    let dir = scenarios_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading scenarios dir {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .context("reading scenarios dir entries")?
        .into_iter()
        .filter(|p| p.extension().map(|x| x == "yaml").unwrap_or(false))
        .collect();
    paths.sort();

    if !sel.is_empty() {
        paths.retain(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|stem| sel.iter().any(|s| s == stem))
                .unwrap_or(false)
        });
        anyhow::ensure!(
            !paths.is_empty(),
            "no scenario matched {sel:?} in {}",
            dir.display()
        );
    }

    let mut out = Vec::new();
    for path in paths {
        let sc = Scenario::load(&path.to_string_lossy())?;
        eprintln!(
            "── {} ({iterations} iterations, {warmup} warmup)",
            sc.name
        );
        out.push(run_one(&sut, &oracle, &sc, iterations, warmup).await?);
    }
    Ok(out)
}

/// One scenario: warmup, then `iterations` measured replays of both sides with
/// the starting side alternating, cleanup unmeasured between iterations.
async fn run_one(
    sut: &NextcloudClient,
    oracle: &NextcloudClient,
    sc: &Scenario,
    iterations: u32,
    warmup: u32,
) -> Result<ScenarioReport> {
    for _ in 0..warmup {
        replay(&sut, sc).await?;
        replay(&oracle, sc).await?;
    }

    // Op names come from the first replay; the op list is identical across
    // sides, so the two maps are keyed the same.
    let mut op_names: Vec<String> = Vec::new();
    let mut rust_times: HashMap<String, Vec<Duration>> = HashMap::new();
    let mut php_times: HashMap<String, Vec<Duration>> = HashMap::new();

    for i in 0..iterations {
        let rust_first = i % 2 == 0;
        let (first, second, first_times, second_times) = if rust_first {
            (&sut, &oracle, &mut rust_times, &mut php_times)
        } else {
            (&oracle, &sut, &mut php_times, &mut rust_times)
        };

        let (first_results, first_vars) = replay(first, sc).await?;
        let (second_results, second_vars) = replay(second, sc).await?;

        if op_names.is_empty() {
            op_names = first_results.iter().map(|r| r.op.clone()).collect();
        }
        for r in &first_results {
            first_times.entry(r.op.clone()).or_default().push(r.elapsed);
        }
        for r in &second_results {
            second_times.entry(r.op.clone()).or_default().push(r.elapsed);
        }

        // Restore pre-scenario state so the next iteration measures the same
        // thing.  Captured values (share ids) are per-side.
        for (client, vars) in [(&sut, &first_vars), (&oracle, &second_vars)] {
            if let Err(e) = scenario::run_cleanup(client, sc, vars).await {
                eprintln!("  warn: cleanup failed for {}: {e:#}", sc.name);
            }
        }
    }

    let mut ops = Vec::new();
    let mut rust_total = Duration::ZERO;
    let mut php_total = Duration::ZERO;
    for name in &op_names {
        let rust = Stats::of(rust_times.get(name).map(Vec::as_slice).unwrap_or_default());
        let php = Stats::of(php_times.get(name).map(Vec::as_slice).unwrap_or_default());
        rust_total += rust.mean;
        php_total += php.mean;
        ops.push(OpStat {
            name: name.clone(),
            treatment: sut_treatment(name),
            rust,
            php,
            ratio: report::ratio(php.mean, rust.mean),
        });
    }

    Ok(ScenarioReport {
        name: sc.name.clone(),
        ops,
        rust_total,
        php_total,
    })
}

/// Replay the scenario's main ops, returning per-op results and the captured
/// values map (share ids etc.) for the cleanup pass.
async fn replay(
    client: &NextcloudClient,
    sc: &Scenario,
) -> Result<(Vec<OpResult>, HashMap<String, String>)> {
    let mut vars = HashMap::new();
    let results = scenario::run(client, sc, &mut vars).await?;
    Ok((results, vars))
}

/// Classify an op's SUT treatment (NATIVE vs PROXY) from its description,
/// mirroring the route table in `nc-server/src/router.rs` (Phase 17.2).
fn sut_treatment(op: &str) -> &'static str {
    // Composite ops: `describe()` emits no path for these.
    if op.starts_with("CHUNKED_V2") || op.starts_with("BULK") {
        return "NATIVE";
    }
    if op.starts_with("SHARE_") || op.starts_with("OCS_") {
        return "PROXY";
    }

    let path = op.split_whitespace().nth(1).unwrap_or(op);

    // Proxied DAV sub-trees — router.rs registers these more-specific routes
    // before the generic `/remote.php/dav/{*path}` wildcard, so they must be
    // checked first here too.
    const PROXY_DAV: &[&str] = &[
        "/remote.php/dav/versions",
        "/remote.php/dav/comments",
        "/remote.php/dav/trashbin",
        "/remote.php/dav/principals",
        "/remote.php/dav/calendars",
        "/remote.php/dav/public-calendars",
        "/remote.php/dav/system-calendars",
        "/remote.php/dav/addressbooks",
        "/remote.php/dav/avatars",
        "/remote.php/dav/access-control",
        "/dav/versions",
        "/dav/comments",
        "/dav/trashbin",
        "/dav/principals",
        "/dav/calendars",
        "/dav/public-calendars",
        "/dav/system-calendars",
        "/dav/addressbooks",
        "/dav/avatars",
        "/dav/access-control",
    ];
    if PROXY_DAV.iter().any(|p| path.starts_with(p)) {
        return "PROXY";
    }

    const NATIVE: &[&str] = &[
        "/status.php",
        "/heartbeat",
        "/remote.php/webdav",
        "/remote.php/dav/files",
        "/dav/files",
        "/remote.php/dav/uploads",
        "/dav/uploads",
        "/remote.php/dav/bulk",
        "/dav/bulk",
        "/core/preview",
        "/apps/files/api/v1/thumbnail",
        "/ocs/v1.php/config",
        "/ocs/v2.php/config",
        "/ocs/v1.php/cloud/capabilities",
        "/ocs/v2.php/cloud/capabilities",
    ];
    if NATIVE.iter().any(|p| path.starts_with(p)) {
        return "NATIVE";
    }

    // Arbiter roots: native for everything except SEARCH/REPORT (proxied).
    if path == "/remote.php/dav" || path == "/remote.php/dav/" || path == "/dav" || path == "/dav/"
    {
        return if op.starts_with("SEARCH") || op.starts_with("REPORT") {
            "PROXY"
        } else {
            "NATIVE"
        };
    }

    "PROXY"
}
