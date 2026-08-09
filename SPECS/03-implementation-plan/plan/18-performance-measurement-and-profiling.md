# 18. Performance Measurement and Profiling: benchmark harness + Rust profiling

## Context

The rewrite's raison d'être is performance (CLAUDE.md core principle 2), yet nothing in the repo currently *measures* it. The differential harness (`nc-difftest`, Phase 16 / plan §16) proves Rust behaves like PHP but not how fast it is. This phase adds the two-layer measurement stack the project needs:

1. **`nc-bench`** — a black-box benchmark harness that replays the existing difftest scenario corpus (plus a load mode) against the SUT (Rust, `:8080`) and the oracle (pure PHP, `:9091`) and reports per-op/per-probe latency percentiles, throughput, and Rust-vs-PHP ratios.
2. **In-process Rust profiling** — `pprof-rs` flamegraph dumps triggered by SIGUSR2 (dev-only, env-gated) + debug-level `tracing` spans for per-handler time breakdowns.

The dev docker already provides the ideal test bed: SUT and oracle are byte-identical images on *separate* databases (`nextcloud` vs `oracle`), so load tests don't contend or cross-contaminate. The oracle (`:9091`) — not the dev PHP entry `:9090` (which shares the SUT's DB) — is the PHP side under test. The php84 image even ships `excimer`/`blackfire`/`xdebug` for future PHP-side profiling (out of scope here).

Reuses: `nc-difftest`'s `Config::from_env()` (`NC_DIFFTEST_*`), `NextcloudClient`, `Scenario::load`, `preconditions::check`, `run_ops`, and the 20+ scenario YAMLs with their cleanup ops (already re-runnable).

---

## Part A — `nc-bench` crate (new workspace member)

### A1. Workspace plumbing
- `core-rs/Cargo.toml`: add `"crates/nc-bench"` to `members`; add `nc-difftest = { path = "crates/nc-difftest" }` to `[workspace.dependencies]` (deps: clap, tokio, anyhow, serde/serde_json for `--json`; reqwest/sqlx come transitively).
- New crate `core-rs/crates/nc-bench/` — binary-only (`src/main.rs`, no lib). Deliberately black-box like difftest: links no `nc-*` server crate.

### A2. Small backward-compatible change in `nc-difftest`
Add `pub elapsed: std::time::Duration` to `OpResult` (src/scenario.rs:181) and measure it in `run_ops` per op. The differential runner reads only `op`/`status`/`body*`, so this is additive. This is the one change to existing code; everything else in difftest stays untouched.

### A3. `bench scenario` — end-to-end latency (sequential)
For each scenario YAML (or `--scenario NAME` subset):
1. `preconditions::check(&cfg)` — fail fast on a broken stack.
2. Warmup: 1 full replay per side (unmeasured) — PHP opcache + Rust caches warm.
3. `--iterations N` (default 5) measured replays. Per iteration, alternate the starting side (Rust-first on even iterations, PHP-first on odd) to cancel drift. Replay via `scenario::run` with per-side `vars` (share-id capture) — mirror the differential runner's flow; then `run_cleanup` unmeasured so iterations stay re-runnable.
4. Each op's `elapsed` is recorded per side; aggregate into p50/p90/p99/mean/max per op.

Report: aligned table per scenario — `op | rust p50/p90/mean | php p50/p90/mean | ratio (php/rust)` — plus a scenario-total row and a per-op SUT-treatment label (`NATIVE` vs `PROXY`) from a small path-prefix classifier mirroring `router.rs` (e.g. `/remote.php/webdav`→native, `/ocs/v2.php/apps/…`→proxy; see `core-rs/crates/nc-server/src/router.rs:115-217`).

### A4. `bench load` — concurrent throughput
Read-only probe requests hammered with `--concurrency C` workers (default 4) for `--duration S` (default 10s) after a `--warmup` (2s). Sides run sequentially (Rust load, then PHP load) with a 1s cooldown — never concurrently, so they don't contend. Per probe: `req/s, p50/p90/p99/max, error count`.

Probes (in-code defaults, all read-only): `GET /status.php`, `GET /ocs/v2.php/cloud/capabilities?format=json`, `PROPFIND /remote.php/webdav/` (depth 0), `PROPFIND /remote.php/webdav/` (depth 1). Arbitrary extra probes via `--probe "METHOD path [depth=N]"` flags.

Worker loop: each worker issues the probe in a tight loop recording `Instant` deltas into a shared channel; aggregator consumes at the end.

### A5. Output & exit codes
- Terminal table; `--json` emits a machine-readable report (kind, per-op/probe stats, ratios) for scripting/CI.
- Exit non-zero if any op/probe errors or a side was unreachable — `make bench` fails loudly, never silently passes a broken stack.

---

## Part B — In-process Rust profiling (`nc-server`)

### B1. pprof-rs flamegraph on SIGUSR2 (dev-only, env-gated)
- Add `pprof = { version = "0.15", features = ["flamegraph", "protobuf"] }` to `nc-server` (verify exact latest version at implementation; `#![forbid(unsafe_code)]` is fine — the crate is a dependency).
- `main.rs`: if `NC_PROFILE_DIR` is set, extend the existing `shutdown_signal` select (src/main.rs:387) with a SIGUSR2 branch. On trigger, spawn a task: `ProfilerGuardBuilder::default().frequency(1000).build()` → sleep `NC_PROFILE_SECS` (default 10) → `report.flamegraph(svg)` + `report.collapse(folded)` into `NC_PROFILE_DIR/profile-<timestamp>.{svg,folded}`. Log start/stop at `info!` with the output paths.
- **Strip caveat (important):** `[profile.release]` sets `strip = true` (Cargo.toml) — pprof would produce unresolved frames. Add `[profile.profiling]` (inherits release, `strip = false`, `debug = "limited"`) to the workspace Cargo.toml so the profiling binary keeps symbols.
- **Trigger mechanics:** PID 1 is `bootstrap.sh`, not nc-server — `docker kill -s USR2` won't reach it. Use `docker exec master-nextcloud-1 bash -c 'pkill -USR2 -x nc-server'` instead.

### B2. Debug-level tracing spans for handler breakdowns
Add spans (all `level = "debug"` — zero cost at the default `info` filter):
- `#[tracing::instrument(skip_all, level = "debug")]` on `nc_dav::handler::dav_handler` (crates/nc-dav/src/handler.rs)
- `span!` in `auth_layer` (middleware/auth.rs), `php_fpm_fallback`/`nc_fastcgi::proxy_handler`, and the native OCS dispatch (nc-ocs router).

Usage: `RUST_LOG=nc_server=debug` → per-request span tree with wall-time per handler in the SUT logs. This is the direct counterpart to an xhprof/excimer breakdown and answers "where does the 5ms go" (auth vs DB vs XML).

---

## Part C — Makefile + docs

### C1. Makefile targets (repo root)
```make
bench:       # cd core-rs && cargo run -p nc-bench --release -- scenario
bench-one:   # … -- scenario --scenario $(SC)          (e.g. SC=10_put_get)
bench-load:  # … -- load
bench-json:  # … -- scenario --json
profile:     # build with --profile profiling → docker cp binary into master-nextcloud-1,
             # restart nc-server in-container, pkill -USR2, wait NC_PROFILE_SECS,
             # docker cp the svg/folded out to ./profiles/
```

### C2. Docs
- `core-rs/docs/benchmarks.md`: methodology (warmup, interleaving, why the oracle `:9091` not `:9090`, shared-host caveat, SUT-proxied ops are measured as-is), commands, probe list, and a baseline results table filled from the first real run.
- `SPECS/04-tasks/phase-17.md`: phase doc following the phase-16 shape (goal, governing decisions, verifiable stops S0–S2 with gates, tasks 17.1+ with checkboxes, `## Changes` log). Per CLAUDE.md conventions: task text written up front, status via checkboxes only.

---

## Files touched

| File | Change |
|---|---|
| `core-rs/Cargo.toml` | workspace member `nc-bench`; `[profile.profiling]`; `nc-difftest` path dep |
| `core-rs/crates/nc-difftest/src/scenario.rs` | `OpResult.elapsed` + timing in `run_ops` (additive) |
| `core-rs/crates/nc-bench/` | new crate: main.rs (clap), scenario.rs, load.rs, report.rs |
| `core-rs/crates/nc-server/Cargo.toml` | `pprof` dep |
| `core-rs/crates/nc-server/src/main.rs` | SIGUSR2 profile-dump branch + `NC_PROFILE_DIR`/`NC_PROFILE_SECS` |
| `core-rs/crates/nc-dav/src/handler.rs`, `nc-server/src/middleware/auth.rs`, `nc-fastcgi` proxy, `nc-ocs` router | debug spans |
| `Makefile` | `bench` / `bench-one` / `bench-load` / `bench-json` / `profile` targets |
| `core-rs/docs/benchmarks.md`, `SPECS/04-tasks/phase-17.md` | new docs |

## Verification

1. `make diff-up` — stack live (both instances `installed:true`).
2. `make bench-one SC=01_propfind_readonly` — table shows sane per-op p50/p90/mean for both sides with ratio; rerun twice → numbers stable (interleaving works).
3. `make bench` — full scenario corpus passes, exit 0; `make bench-load` — req/s and percentiles for all 4 probes.
4. `cargo test --lib` in `core-rs` — unchanged behavior (OpResult change is additive; differential suite is `#[ignore]`d anyway).
5. `make profile` — `profiles/profile-*.svg` opens with **resolved function names** (proves the `strip=false` profiling profile works); `docker logs master-nextcloud-1` shows the profile span tree under `RUST_LOG=nc_server=debug`.
6. Fill the baseline table in `docs/benchmarks.md` from a real run.
