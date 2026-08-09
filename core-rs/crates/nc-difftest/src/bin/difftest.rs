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

/// Quiesce the async background writers before a snapshot.
///
/// The dev containers run cron on a 5-minute schedule (`*/5` in
/// docker/configs/cron.conf); the previewgenerator's PreviewJob drains
/// `oc_preview_generation` on that cadence, and the job loop bumps the
/// heartbeat appconfig rows.  A snapshot straddling one of those events
/// diverges on which side the event landed in the window.  Instead of racing
/// the schedule (or masking the rows in divergences.yaml), force each side's
/// PreviewJob (`occ background-job:execute … --force-execute`) so the queue
/// drains deterministically — the job's writes land before the snapshot on
/// both sides, so the deltas stay clean.  Fast path: both queues already
/// empty (the common case between scenarios) → no job runs at all.  Bounded:
/// a stuck job only warns (the residual event is covered by the
/// background-job-heartbeat noise records).
async fn quiesce_background(cfg: &Config) -> Result<()> {
    let s0 = db::count_table(&cfg.sut.dsn, "oc_preview_generation").await?;
    let o0 = db::count_table(&cfg.oracle.dsn, "oc_preview_generation").await?;
    if s0 == 0 && o0 == 0 {
        return Ok(());
    }
    for (inst, dsn) in [(&cfg.sut, &cfg.sut.dsn), (&cfg.oracle, &cfg.oracle.dsn)] {
        let Some(job_id) = db::preview_job_id(dsn).await? else {
            tracing::warn!(container = %inst.container, "no PreviewJob in oc_jobs — skipping drain");
            continue;
        };
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::process::Command::new("docker")
                .args([
                    "exec",
                    &inst.container,
                    "sudo", "-E", "-u", "www-data",
                    "php", "/var/www/html/occ",
                    "background-job:execute",
                    &job_id.to_string(),
                    "--force-execute",
                ])
                .output(),
        )
        .await;
        match res {
            Ok(Ok(o)) if o.status.success() => {}
            Ok(Ok(o)) => tracing::warn!(
                container = %inst.container,
                status = %o.status,
                "background-job:execute exited non-zero"
            ),
            Ok(Err(e)) => tracing::warn!(container = %inst.container, error = %e, "docker exec background-job:execute failed"),
            Err(_) => tracing::warn!(container = %inst.container, "background-job:execute timed out after 60s"),
        }
    }
    let mut last = (1i64, 1i64);
    for _ in 0..30 {
        let s = db::count_table(&cfg.sut.dsn, "oc_preview_generation").await?;
        let o = db::count_table(&cfg.oracle.dsn, "oc_preview_generation").await?;
        last = (s, o);
        if s == 0 && o == 0 {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    tracing::warn!(
        sut = last.0,
        oracle = last.1,
        "oc_preview_generation did not drain within 30s"
    );
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

    quiesce_background(&cfg).await?;

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
    let mut bytes_ok = true;
    for ((a, b), op) in sut_res.iter().zip(oracle_res.iter()).zip(sc.ops.iter()) {
        if !op.compare_status() {
            println!(
                "  {}: SUT {} vs oracle {} (status comparison skipped — recorded divergence)",
                a.op, a.status, b.status
            );
        } else if a.status != b.status {
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
        // Byte-exact parity (Phase 16.11 Imaginary): generated preview bytes
        // must be identical — both sides POST to the same Imaginary.
        if let (Some(ab), Some(ob)) = (&a.body_bytes, &b.body_bytes) {
            if ab != ob {
                println!(
                    "  BYTES MISMATCH {}: SUT {} B vs oracle {} B",
                    a.op,
                    ab.len(),
                    ob.len()
                );
                bytes_ok = false;
            } else {
                println!("  {}: bytes identical ({} B)", a.op, ab.len());
            }
        }
    }
    let ops_ok = status_ok && body_ok && bytes_ok;

    // Quiesce the async background writers again: the ops queued preview
    // generations (and the 5-min container cron may have fired mid-window) —
    // trigger both sides' cron and drain the queue so the after-snapshot sees
    // the converged state (same principle as the idle double-snapshot check).
    quiesce_background(&cfg).await?;

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
        if !bytes_ok {
            println!("  (preview-byte mismatch above — Imaginary parity)");
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
