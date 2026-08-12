//! `bench load` — concurrent throughput on read-only probes.
//!
//! Each probe is hammered by `--concurrency` workers for `--duration` seconds,
//! sides run **sequentially** (Rust first, then PHP, with a cooldown) so the
//! two implementations never contend for the shared Postgres/Redis and the
//! comparison stays clean.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nc_difftest::client::NextcloudClient;
use nc_difftest::config::Config;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Method;

use crate::auth;
use crate::report::{LoadReport, LoadStats, ProbeStat, Stats};

/// One read-only probe request.
#[derive(Debug, Clone)]
pub struct Probe {
    pub method: String,
    pub path: String,
    pub depth: Option<u32>,
}

impl Probe {
    pub fn describe(&self) -> String {
        match self.depth {
            Some(d) => format!("{} {} (depth {d})", self.method, self.path),
            None => format!("{} {}", self.method, self.path),
        }
    }

    /// Parse `"METHOD path [depth=N]"` (extra `--probe` flags).
    pub fn parse(s: &str) -> Result<Probe> {
        let mut it = s.split_whitespace();
        let method = it
            .next()
            .context("probe must start with the HTTP method")?
            .to_string();
        let path = it.next().context("probe must include a path")?.to_string();
        let mut depth = None;
        for kv in it {
            let (k, v) = kv
                .split_once('=')
                .context("probe flags are `key=value`, e.g. depth=1")?;
            match k {
                "depth" => depth = Some(v.parse().context("depth must be an integer")?),
                other => anyhow::bail!("unknown probe flag {other:?}"),
            }
        }
        Ok(Probe {
            method,
            path,
            depth,
        })
    }

    fn headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(d) = self.depth {
            h.insert("Depth", HeaderValue::from(d));
        }
        h
    }
}

/// The default probe set — all read-only, covering the always-on native
/// endpoints plus the native DAV files tree (the hot path).
pub fn default_probes() -> Vec<Probe> {
    vec![
        Probe {
            method: "GET".into(),
            path: "/status.php".into(),
            depth: None,
        },
        Probe {
            method: "GET".into(),
            path: "/ocs/v2.php/cloud/capabilities?format=json".into(),
            depth: None,
        },
        Probe {
            method: "PROPFIND".into(),
            path: "/remote.php/webdav/".into(),
            depth: Some(0),
        },
        Probe {
            method: "PROPFIND".into(),
            path: "/remote.php/webdav/".into(),
            depth: Some(1),
        },
    ]
}

/// Measure each probe: warmup both sides, then Rust then PHP (sequential).
pub async fn bench(
    cfg: &Config,
    extra: &[String],
    concurrency: usize,
    duration_secs: u64,
    warmup_secs: u64,
) -> Result<LoadReport> {
    nc_difftest::preconditions::check(cfg).await.context(
        "preconditions failed — is the stack up (`make diff-up`) and the sides identical?",
    )?;

    let mut probes = default_probes();
    for s in extra {
        probes.push(Probe::parse(s)?);
    }

    // Clear throttler state, then authenticate with per-instance app tokens
    // (hot path — see `auth`).
    auth::reset_throttle(&cfg.sut).await;
    auth::reset_throttle(&cfg.oracle).await;
    let (sut_user, sut_pass) =
        auth::instance_creds(&cfg.sut, &cfg.admin_user, &cfg.admin_pass).await?;
    let (oracle_user, oracle_pass) =
        auth::instance_creds(&cfg.oracle, &cfg.admin_user, &cfg.admin_pass).await?;

    let sut = Arc::new(NextcloudClient::new(&cfg.sut, &sut_user, &sut_pass)?);
    let oracle = Arc::new(NextcloudClient::new(
        &cfg.oracle,
        &oracle_user,
        &oracle_pass,
    )?);

    let mut out = Vec::new();
    for probe in &probes {
        eprintln!("── probe {}", probe.describe());

        // Warm both sides so caches/opcache are hot before measuring.
        hammer(&sut, probe, Duration::from_secs(warmup_secs), concurrency).await?;
        hammer(
            &oracle,
            probe,
            Duration::from_secs(warmup_secs),
            concurrency,
        )
        .await?;

        let rust = hammer(&sut, probe, Duration::from_secs(duration_secs), concurrency).await?;
        // Cooldown so the two measured runs don't bleed into each other.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let php = hammer(
            &oracle,
            probe,
            Duration::from_secs(duration_secs),
            concurrency,
        )
        .await?;

        // Same semantics as scenario mode: >1 means Rust is faster
        // (rust req/s over php req/s).
        let ratio_reqs = if php.reqs_per_sec() > 0.0 {
            rust.reqs_per_sec() / php.reqs_per_sec()
        } else {
            f64::NAN
        };
        out.push(ProbeStat {
            probe: probe.describe(),
            rust,
            php,
            ratio_reqs,
        });
    }
    Ok(LoadReport { probes: out })
}

/// One side's hammer run: `concurrency` workers each issue the probe in a
/// tight loop until `duration` elapses, streaming `Instant` deltas back.
/// Transport errors are counted, not fatal — a load test's job is to report
/// how many requests failed, not to die on the first one.
async fn hammer(
    client: &Arc<NextcloudClient>,
    probe: &Probe,
    duration: Duration,
    concurrency: usize,
) -> Result<LoadStats> {
    let method = Method::from_bytes(probe.method.as_bytes())
        .with_context(|| format!("invalid method {:?}", probe.method))?;
    let headers = probe.headers();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<Duration, ()>>();
    let started = Instant::now();
    for _ in 0..concurrency {
        let tx = tx.clone();
        let client = Arc::clone(client);
        let probe = probe.clone();
        let method = method.clone();
        let headers = headers.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + duration;
            while Instant::now() < deadline {
                let t0 = Instant::now();
                let resp = client
                    .request(method.clone(), &probe.path, headers.clone(), None)
                    .await;
                match resp {
                    Ok(_) => {
                        let _ = tx.send(Ok(t0.elapsed()));
                    }
                    Err(_) => {
                        let _ = tx.send(Err(()));
                    }
                }
            }
        });
    }
    drop(tx);

    let mut latencies = Vec::new();
    let mut errors = 0u64;
    while let Some(sample) = rx.recv().await {
        match sample {
            Ok(d) => latencies.push(d),
            Err(()) => errors += 1,
        }
    }
    let wall = started.elapsed();

    Ok(LoadStats {
        stats: Stats::of(&latencies),
        errors,
        wall,
    })
}
