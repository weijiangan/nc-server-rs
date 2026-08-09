//! Report data structures + rendering (terminal tables and `--json`).
//!
//! All durations are converted to milliseconds for both the tables and the
//! JSON output so the two formats agree.

use std::time::Duration;

use serde_json::json;

// ── Statistics ───────────────────────────────────────────────────────────────

/// Per-op / per-probe latency aggregate over the measured samples.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub count: u32,
    pub p50: Duration,
    pub p90: Duration,
    pub p99: Duration,
    pub mean: Duration,
    pub max: Duration,
}

impl Stats {
    pub fn of(samples: &[Duration]) -> Stats {
        let count = samples.len() as u32;
        if samples.is_empty() {
            return Stats {
                count: 0,
                p50: Duration::ZERO,
                p90: Duration::ZERO,
                p99: Duration::ZERO,
                mean: Duration::ZERO,
                max: Duration::ZERO,
            };
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        Stats {
            count,
            p50: percentile(&sorted, 0.50),
            p90: percentile(&sorted, 0.90),
            p99: percentile(&sorted, 0.99),
            mean: sorted.iter().sum::<Duration>() / count,
            max: *sorted.last().expect("non-empty"),
        }
    }
}

/// Nearest-rank percentile over a sorted slice (clamped to the last sample).
fn percentile(sorted: &[Duration], q: f64) -> Duration {
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Speedup ratio: PHP mean / Rust mean. >1 means Rust is faster.
pub fn ratio(php: Duration, rust: Duration) -> f64 {
    if rust.is_zero() {
        f64::NAN
    } else {
        php.as_secs_f64() / rust.as_secs_f64()
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn fmt_ms(d: Duration) -> String {
    format!("{:.2}", ms(d))
}

fn fmt_ratio(r: f64) -> String {
    if r.is_finite() {
        format!("{r:.2}x")
    } else {
        "--".to_string()
    }
}

fn stats_json(s: &Stats) -> serde_json::Value {
    json!({
        "count": s.count,
        "p50_ms": ms(s.p50),
        "p90_ms": ms(s.p90),
        "p99_ms": ms(s.p99),
        "mean_ms": ms(s.mean),
        "max_ms": ms(s.max),
    })
}

// ── Scenario mode ────────────────────────────────────────────────────────────

pub struct OpStat {
    pub name: String,
    /// SUT treatment: "NATIVE" (Rust handles it) or "PROXY" (FastCGI to PHP).
    pub treatment: &'static str,
    pub rust: Stats,
    pub php: Stats,
    pub ratio: f64,
}

pub struct ScenarioReport {
    pub name: String,
    pub ops: Vec<OpStat>,
    /// Mean per-iteration wall time (sum of per-op means; every iteration runs
    /// every op, so Σ mean(op) == mean(Σ op)).
    pub rust_total: Duration,
    pub php_total: Duration,
}

pub fn render_scenarios(reps: &[ScenarioReport], json: bool) {
    if json {
        let scenarios: Vec<serde_json::Value> = reps
            .iter()
            .map(|r| {
                json!({
                    "scenario": r.name,
                    "rust_total_ms": ms(r.rust_total),
                    "php_total_ms": ms(r.php_total),
                    "ratio": ratio(r.php_total, r.rust_total),
                    "ops": r.ops.iter().map(|o| json!({
                        "op": o.name,
                        "treatment": o.treatment,
                        "rust": stats_json(&o.rust),
                        "php": stats_json(&o.php),
                        "ratio": o.ratio,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "kind": "scenario", "scenarios": scenarios }))
                .expect("serializing JSON report")
        );
        return;
    }

    for r in reps {
        println!("\nScenario {}", r.name);
        println!(
            "  {:<46} {:<7} {:>22} {:>22} {:>8}",
            "op", "mode", "rust p50/p90/mean", "php p50/p90/mean", "ratio"
        );
        for o in &r.ops {
            let name = truncate(&o.name, 46);
            println!(
                "  {:<46} {:<7} {:>22} {:>22} {:>8}",
                name,
                o.treatment,
                triplet(&o.rust),
                triplet(&o.php),
                fmt_ratio(o.ratio),
            );
        }
        println!(
            "  {:<46} {:<7} {:>22} {:>22} {:>8}",
            "TOTAL",
            "",
            fmt_ms(r.rust_total),
            fmt_ms(r.php_total),
            fmt_ratio(ratio(r.php_total, r.rust_total)),
        );
    }
}

/// "p50/p90/mean" in ms.
fn triplet(s: &Stats) -> String {
    format!("{}/{}/{}", fmt_ms(s.p50), fmt_ms(s.p90), fmt_ms(s.mean))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 3).collect();
        out.push('…');
        out
    }
}

// ── Load mode ────────────────────────────────────────────────────────────────

pub struct LoadStats {
    pub stats: Stats,
    /// Failed (transport-level) requests.
    pub errors: u64,
    /// Measured wall clock of the hammer run.
    pub wall: Duration,
}

impl LoadStats {
    pub fn reqs_per_sec(&self) -> f64 {
        if self.wall.is_zero() {
            f64::NAN
        } else {
            self.stats.count as f64 / self.wall.as_secs_f64()
        }
    }
}

pub struct ProbeStat {
    pub probe: String,
    pub rust: LoadStats,
    pub php: LoadStats,
    /// Rust req/s / PHP req/s — >1 means Rust is faster (same convention as
    /// scenario mode's latency ratio).
    pub ratio_reqs: f64,
}

pub struct LoadReport {
    pub probes: Vec<ProbeStat>,
}

pub fn render_load(rep: &LoadReport, json: bool) {
    if json {
        let probes: Vec<serde_json::Value> = rep
            .probes
            .iter()
            .map(|p| {
                let side = |l: &LoadStats| {
                    json!({
                        "reqs": l.stats.count,
                        "reqs_per_sec": l.reqs_per_sec(),
                        "errors": l.errors,
                        "wall_secs": l.wall.as_secs_f64(),
                        "latency_ms": stats_json(&l.stats),
                    })
                };
                json!({
                    "probe": p.probe,
                    "rust": side(&p.rust),
                    "php": side(&p.php),
                    "rust_to_php_reqs_per_sec": p.ratio_reqs,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "kind": "load", "probes": probes }))
                .expect("serializing JSON report")
        );
        return;
    }

    for p in &rep.probes {
        println!("\nProbe {}", p.probe);
        println!(
            "  {:<6} {:>10} {:>24} {:>10} {:>8}",
            "side", "req/s", "p50/p90/mean (ms)", "max (ms)", "errors"
        );
        println!("  {:<6} {:>10} {:>24} {:>10} {:>8}", "rust", fmt_reqs(p.rust.reqs_per_sec()), triplet(&p.rust.stats), fmt_ms(p.rust.stats.max), p.rust.errors);
        println!("  {:<6} {:>10} {:>24} {:>10} {:>8}", "php", fmt_reqs(p.php.reqs_per_sec()), triplet(&p.php.stats), fmt_ms(p.php.stats.max), p.php.errors);
        println!(
            "  {:<6} {:>10} {:>24} {:>10} {:>8}",
            "ratio",
            fmt_ratio(p.ratio_reqs),
            "",
            "",
            "",
        );
    }
}

fn fmt_reqs(r: f64) -> String {
    if r.is_finite() {
        format!("{r:.1}")
    } else {
        "--".to_string()
    }
}
