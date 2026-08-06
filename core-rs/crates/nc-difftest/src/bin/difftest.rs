//! `difftest` — CLI for the differential harness.
//!
//! - `smoke`:    preconditions + PROPFIND Depth:0 on both (Phase 16.2).
//! - `snapshot`: 16.3 snapshot parity/idle check.
//! - `run`:      replay a scenario on both sides and diff the DB deltas (16.4+).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nc_difftest::{
    canonicalize::{Canonicalizer, Registry},
    client::NextcloudClient,
    config::Config,
    db, delta, fs, preconditions, report,
    scenario::Scenario,
};

#[derive(Parser)]
#[command(name = "difftest", about = "Differential integration-test harness (Phase 16)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Preconditions + PROPFIND Depth:0 on the home root against both instances;
    /// assert both answer 207.
    Smoke,
    /// Phase 16.3: snapshot both DBs, assert table-set parity, report core
    /// tables, and check that an idle double-snapshot is identical.
    Snapshot,
    /// Phase 16.4: replay a scenario on both sides, snapshot before/after, and
    /// diff the canonical DB deltas. Non-empty diff = failure.
    Run {
        /// Path to a scenario YAML (e.g. crates/nc-difftest/scenarios/10_put_get_delete.yaml).
        scenario: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let cfg = Config::from_env().context("loading config (NC_DIFFTEST_*)")?;
    match cli.cmd {
        Cmd::Smoke => smoke(&cfg).await,
        Cmd::Snapshot => snapshot_check(&cfg).await,
        Cmd::Run { scenario } => run_scenario(&cfg, &scenario).await,
    }
}

fn registry_path() -> String {
    format!("{}/column_registry.yaml", env!("CARGO_MANIFEST_DIR"))
}

async fn smoke(cfg: &Config) -> Result<()> {
    preconditions::check(cfg).await?;
    println!("preconditions OK: same numeric version + enabled-app set");

    let path = format!("/remote.php/dav/files/{}/", cfg.admin_user);
    for (label, inst) in [("SUT", &cfg.sut), ("Oracle", &cfg.oracle)] {
        let client = NextcloudClient::new(inst, &cfg.admin_user, &cfg.admin_pass)?;
        let resp = client.propfind(&path, 0, None).await?;
        let status = resp.status();
        let body = resp.text().await?;
        let responses = body.matches("<d:response>").count();
        println!(
            "{label:6} PROPFIND Depth:0 {path} -> {status} ({responses} response element(s), {} bytes)",
            body.len()
        );
        anyhow::ensure!(
            status.as_u16() == 207,
            "{label} PROPFIND returned {status}, expected 207"
        );
    }

    println!("smoke OK: both instances serve the files tree");
    Ok(())
}

async fn snapshot_check(cfg: &Config) -> Result<()> {
    println!("snapshotting SUT ({}) ...", cfg.sut.dsn);
    let sut = db::snapshot(&cfg.sut.dsn).await?;
    println!("re-snapshotting SUT (idle check) ...");
    let sut2 = db::snapshot(&cfg.sut.dsn).await?;
    if sut != sut2 {
        println!("idle double-snapshot DIVERGED — residual background writes in:");
        for (name, a) in sut.tables.iter() {
            if let Some(b) = sut2.tables.get(name) {
                if a != b {
                    println!("  {name}: {} rows -> {} rows", a.rows.len(), b.rows.len());
                }
            }
        }
        anyhow::bail!("idle snapshot not stable — extend the skip-list/masking (Phase 16.5)");
    }
    println!("idle double-snapshot: IDENTICAL ({} tables, quiesced)", sut.tables.len());

    println!("snapshotting Oracle ({}) ...", cfg.oracle.dsn);
    let oracle = db::snapshot(&cfg.oracle.dsn).await?;
    db::assert_table_parity(&sut, &oracle)?;
    println!("table-set parity OK ({} tables each)", sut.tables.len());

    for t in ["oc_filecache", "oc_storages", "oc_mimetypes"] {
        let s = sut.tables.get(t).with_context(|| format!("SUT missing {t}"))?;
        let o = oracle
            .tables
            .get(t)
            .with_context(|| format!("oracle missing {t}"))?;
        anyhow::ensure!(!s.rows.is_empty(), "SUT {t} is empty");
        anyhow::ensure!(!o.rows.is_empty(), "oracle {t} is empty");
        println!("  {t}: SUT {} rows, oracle {} rows", s.rows.len(), o.rows.len());
    }

    // Phase 16.8: the file tree must also be quiesced when idle.
    println!("[fs] idle double-snapshot check ...");
    for (label, inst) in [("SUT", &cfg.sut), ("Oracle", &cfg.oracle)] {
        let t1 = fs::snapshot_tree(inst, &cfg.data_dir, &cfg.admin_user).await?;
        let t2 = fs::snapshot_tree(inst, &cfg.data_dir, &cfg.admin_user).await?;
        anyhow::ensure!(
            t1 == t2,
            "{label} idle file-tree snapshot not stable — residual background writes"
        );
        println!("  {label}: {} files, idle double-snapshot IDENTICAL", t1.len());
    }

    println!("snapshot OK");
    Ok(())
}

async fn run_scenario(cfg: &Config, path: &str) -> Result<()> {
    preconditions::check(cfg).await?;
    let sc = Scenario::load(path)?;
    let mut registry = Registry::load(&registry_path())?;
    // Scenario-level overrides: client-dictated values are deterministic, so a
    // masked column can be compared verbatim for this scenario.
    for o in &sc.stable_overrides {
        let (table, col) = o
            .split_once('.')
            .with_context(|| format!("bad stable_override {o:?} (want table.column)"))?;
        registry.set_class(table, col, nc_difftest::canonicalize::Class::Stable);
        println!("[override] {table}.{col} -> stable (client-dictated)");
    }
    let canon = Canonicalizer::new(registry);

    let sut = NextcloudClient::new(&cfg.sut, &cfg.admin_user, &cfg.admin_pass)?;
    let oracle = NextcloudClient::new(&cfg.oracle, &cfg.admin_user, &cfg.admin_pass)?;

    println!("[before] snapshotting both (DB + file tree) ...");
    let sut_before = db::snapshot(&cfg.sut.dsn).await?;
    let oracle_before = db::snapshot(&cfg.oracle.dsn).await?;
    let fs_sut_before = fs::snapshot_tree(&cfg.sut, &cfg.data_dir, &cfg.admin_user).await?;
    let fs_oracle_before = fs::snapshot_tree(&cfg.oracle, &cfg.data_dir, &cfg.admin_user).await?;

    println!("[ops] replaying '{}' ({} ops) ...", sc.name, sc.ops.len());
    // Captured values are per-side: share ids (and any future captures) differ
    // between the SUT and Oracle DBs.
    let (mut sut_vars, mut oracle_vars) =
        (std::collections::HashMap::new(), std::collections::HashMap::new());
    let sut_res = nc_difftest::scenario::run(&sut, &sc, &mut sut_vars).await?;
    let oracle_res = nc_difftest::scenario::run(&oracle, &sc, &mut oracle_vars).await?;
    let mut status_ok = true;
    let mut body_ok = true;
    for (a, b) in sut_res.iter().zip(oracle_res.iter()) {
        if a.status != b.status {
            println!("  STATUS MISMATCH {}: SUT {} vs oracle {}", a.op, a.status, b.status);
            status_ok = false;
        } else {
            println!("  {}: {} == {}", a.op, a.status, b.status);
        }
        // Rejection parity (Phase 16.10): same status is necessary but not
        // sufficient — the error body shape must match too.
        if let (Some(ab), Some(ob)) = (&a.body, &b.body) {
            if ab.trim() != ob.trim() {
                println!(
                    "  BODY MISMATCH {}:\n--- SUT body ---\n{ab}\n--- Oracle body ---\n{ob}",
                    a.op
                );
                body_ok = false;
            }
        }
    }
    let ops_ok = status_ok && body_ok;

    println!("[after] snapshotting both (DB + file tree) ...");
    let sut_after = db::snapshot(&cfg.sut.dsn).await?;
    let oracle_after = db::snapshot(&cfg.oracle.dsn).await?;
    let fs_sut_after = fs::snapshot_tree(&cfg.sut, &cfg.data_dir, &cfg.admin_user).await?;
    let fs_oracle_after = fs::snapshot_tree(&cfg.oracle, &cfg.data_dir, &cfg.admin_user).await?;

    // Cleanup ops run AFTER the after-snapshot so they never enter the diff;
    // they restore pre-scenario state so the scenario is re-runnable. Always
    // attempted, also when the ops themselves diverged.
    if !sc.cleanup.is_empty() {
        println!("[cleanup] replaying {} cleanup op(s) ...", sc.cleanup.len());
        if let Err(e) = nc_difftest::scenario::run_cleanup(&sut, &sc, &sut_vars).await {
            println!("  SUT {e}");
        }
        if let Err(e) = nc_difftest::scenario::run_cleanup(&oracle, &sc, &oracle_vars).await {
            println!("  Oracle {e}");
        }
    }

    println!("[canon] canonicalizing + diffing deltas ...");
    let csb = canon.canonicalize(&sut_before)?;
    let csa = canon.canonicalize(&sut_after)?;
    let cob = canon.canonicalize(&oracle_before)?;
    let coa = canon.canonicalize(&oracle_after)?;

    let d_sut = delta::normalize_delta(delta::delta(&csb, &csa), &canon.registry);
    let d_oracle = delta::normalize_delta(delta::delta(&cob, &coa), &canon.registry);
    let (db_identical, db_diff_text) = report::diff(&d_sut, &d_oracle);

    // Match any divergence against the known-divergence inventory
    // (divergences.yaml — Phase 16.12): listed divergences are reported as
    // KNOWN (with the inventory id and rationale) and do not fail the run;
    // unlisted ones are real failures.
    let inventory_path = format!("{}/divergences.yaml", env!("CARGO_MANIFEST_DIR"));
    let inventory = nc_difftest::divergences::Inventory::load(&inventory_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let divs = nc_difftest::delta::divergences(&d_sut, &d_oracle);
    let (known, unlisted) = inventory.match_run(&sc.name, &divs);

    println!("[fs] diffing file-tree deltas ...");
    let fd_sut = fs::delta(&fs_sut_before, &fs_sut_after);
    let fd_oracle = fs::delta(&fs_oracle_before, &fs_oracle_after);
    let (fs_identical, fs_diff_text) = fs::diff(&fd_sut, &fd_oracle);

    // A DB delta is acceptable when it is identical OR every divergence is
    // covered by the known-divergence inventory (divergences.yaml).
    let db_ok = db_identical || unlisted.is_empty();
    if fs_identical && ops_ok && db_ok {
        if !known.is_empty() {
            println!(
                "KNOWN DIVERGENCES in scenario '{}' (inventory: {}) — documented, not failures:",
                sc.name,
                known.len()
            );
            for (d, rec) in &known {
                println!(
                    "  [{}] {}\n      {}.{} columns {:?}\n      why: {}{}",
                    rec.status,
                    rec.id,
                    d.table,
                    d.key,
                    d.columns,
                    rec.why,
                    rec.revisit
                        .as_ref()
                        .map(|r| format!("\n      revisit: {r}"))
                        .unwrap_or_default()
                );
            }
        }
        println!("IDENTICAL: scenario '{}' produced matching deltas on both sides.", sc.name);
        Ok(())
    } else {
        println!("DIVERGENCE in scenario '{}':", sc.name);
        if !status_ok {
            println!("  (HTTP status mismatch above)");
        }
        if !body_ok {
            println!("  (response-body mismatch above — rejection parity)");
        }
        if !db_identical {
            if !unlisted.is_empty() {
                println!("  UNLISTED divergences (missing from divergences.yaml — real failures):");
                for d in &unlisted {
                    println!("    {}.{} columns {:?}", d.table, d.key, d.columns);
                }
            }
            println!("{db_diff_text}");
        }
        if !fs_identical {
            println!("{fs_diff_text}");
        }
        anyhow::bail!("scenario '{}' diverged", sc.name);
    }
}
