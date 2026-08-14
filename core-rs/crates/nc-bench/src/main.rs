//! `nc-bench` — black-box benchmark harness (Phase 17).
//!
//! Measures the Rust `nc-server` (SUT, `:8080`) against the pure-PHP oracle
//! (`:9091`) on the same dev stack:
//!
//! - `nc-bench scenario`  — replay the nc-difftest scenario corpus and compare
//!   per-op latency percentiles (p50/p90/p99/mean) between the sides.
//! - `nc-bench load`      — hammer read-only probes concurrently and compare
//!   throughput (req/s) and latency percentiles.
//!
//! Like nc-difftest, this crate is deliberately **black-box**: it speaks HTTP
//! through the same `NextcloudClient` on both sides (identical headers, bodies,
//! keep-alive) and links no `nc-*` server crate, so the comparison is fair by
//! construction.  Configuration comes from the same `NC_DIFFTEST_*` env vars.

mod auth;
mod budget;
mod load;
mod php;
mod report;
mod scenario;

use anyhow::Result;
use clap::{Parser, Subcommand};

use nc_difftest::config::Config;

#[derive(Parser, Debug)]
#[command(
    name = "nc-bench",
    about = "Benchmark the Rust nc-server against the pure-PHP oracle (Phase 17)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Replay difftest scenarios; compare per-op latency percentiles.
    Scenario {
        /// Only run scenarios whose file stem matches (repeatable),
        /// e.g. `--scenario 10_put_get`.
        #[arg(long)]
        scenario: Vec<String>,
        /// Measured iterations per scenario (default 5).
        #[arg(long, default_value_t = 5)]
        iterations: u32,
        /// Unmeasured warmup replays per side (default 1).
        #[arg(long, default_value_t = 1)]
        warmup: u32,
        /// Emit a machine-readable JSON report on stdout (progress goes to stderr).
        #[arg(long)]
        json: bool,
    },
    /// Hammer read-only probes concurrently; compare throughput.
    Load {
        /// Extra probes, `"METHOD path [depth=N]"` (repeatable), e.g.
        /// `--probe "GET /status.php"` `--probe "PROPFIND /remote.php/webdav/ depth=1"`.
        #[arg(long)]
        probe: Vec<String>,
        /// Concurrent workers per side (default 4).
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Measured duration in seconds per side (default 10).
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// Unmeasured warmup in seconds per side (default 2).
        #[arg(long, default_value_t = 2)]
        warmup: u64,
        /// Emit a machine-readable JSON report on stdout (progress goes to stderr).
        #[arg(long)]
        json: bool,
    },
    /// Phase 20: run the query-count budget gate against the SUT.
    /// Fails (non-zero exit) when any request class exceeds its budget.
    Budget {
        /// Path to the budget file (`perf-budget.yaml`).
        #[arg(long, default_value = "perf-budget.yaml")]
        budget: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    match cli.cmd {
        Command::Scenario {
            scenario,
            iterations,
            warmup,
            json,
        } => {
            let reps = scenario::bench(&cfg, &scenario, iterations, warmup).await?;
            report::render_scenarios(&reps, json);
        }
        Command::Load {
            probe,
            concurrency,
            duration,
            warmup,
            json,
        } => {
            let rep = load::bench(&cfg, &probe, concurrency, duration, warmup).await?;
            report::render_load(&rep, json);
        }
        Command::Budget { budget } => {
            let pass = budget::run(&cfg, &budget).await?;
            if !pass {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
