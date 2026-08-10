# 20. Performance budget gate — query-count regression guard

## Context

The query-reduction rounds (18.1-18.4) removed ~1,100 statements per 100-child
depth-1 PROPFIND and established measured floors for every request class. Those
floors are currently protected only by review discipline — nothing fails a
build if a future change reintroduces per-child lookups (the N+1 pattern this
project was built to eliminate). This phase adds a **budget gate** in the
spirit of a JS bundle-size budget: a machine-readable budget file, a `make
perf-gate` target that measures the live stack against it, and a non-zero exit
on breach.

The keystone invariant is not "≤ N queries per file" but **0 queries per
child**: depth-1 PROPFIND on an N-child directory must cost `base + 8`
statements (7 batch queries + JOIN listing + tag prefetch), never `base + 11N`.
Any reintroduced per-child query breaches the scaling delta at N ≥ 1.

## Budgets (STRICT: budgets = current measured counts, no headroom)

Source of truth: `core-rs/perf-budget.yaml`. The gate is a hard ceiling —
any statement added to a request class fails `make perf-gate`; reducing a
count is the only way to lower a budget.

| class | request | measured | budget |
|---|---|---|---|
| status | `GET /status.php` | 0 | 0 |
| get_file | `GET /remote.php/webdav/hello.txt` | 5 | 5 |
| propfind_depth0 | `PROPFIND /remote.php/webdav/` depth 0 | 11 | 11 |
| propfind_depth1 | `PROPFIND /remote.php/webdav/` depth 1 | 20 | 20 |
| put_new | `PUT /remote.php/webdav/q-budget-<ts>.txt` | 16 | 16 |
| **scaling delta** | depth1 − depth0 | 9 | **9** |

The scaling delta is the regression detector: depth-1's extra cost over
depth-0 is exactly the fixed batch cost (7 batch queries + JOIN listing +
tag prefetch) — any per-child query reintroduction breaches it at N ≥ 1.
Measured with app-token auth, warm caches, median-of-3 windows.

## Measurement mechanism (Phase 1)

`nc-bench` gains a `budget` subcommand (black-box like the rest of the harness —
links no `nc-*` server crate):

1. Load `perf-budget.yaml`.
2. Create an app token on the SUT (existing `auth::create_token`).
3. Enable `log_statement='all'` on the SUT's Postgres (superuser DSN).
4. Per class: one unmeasured warmup request + 800 ms settle (lets the
   fire-and-forget `last_activity` write land before the window) → three
   measured requests, each inside its own counting window → median-of-3.
   Counting counts `execute sqlx` lines whose **PG log timestamp**
   (millisecond precision) falls inside the window — not podman's ingestion
   time, which lags and batches — so the window is exact.
   - The `execute sqlx` filter counts only Rust's prepared statements;
     PHP/Doctrine logs as `<unnamed>` and background cron is excluded by
     construction.
   - **The probe must consume the response body**: dav-server-rs streams
     PROPFIND responses, and `read_dir` + the per-child batch queries run
     inside the stream — dropping the response without reading it skips the
     very work being measured (the gate's original silent undercount).
5. PUT uses a unique path per run and cleans it up after the window.
6. `depth1 − depth0` scaling-delta check.
7. Disable logging; print the table; exit non-zero on any breach.

Gate wiring: `make perf-gate` (prerequisites: `make diff-up`-style stack up).

## Phase 2 (decided: NOT an in-process counter)

An in-process per-request query counter was originally proposed as the
production guard. Decision (2026-08-10): **dropped.** The dev-stack gate is
the enforcement point — every change ships through `make perf-gate`, so a
regression fails before it reaches a deployment. A permanent counter would
instrument the hottest code in the project (a counting `Executor` wrapper
around the pool used by every query) for a signal that is only a proxy — on
slow storage the query count already surfaces as latency, which `perf` and
the slow-query log measure directly.

Deployed-server verification is instead a **one-shot check after each
deploy**: on the production Postgres, `pg_stat_statements_reset()`, run one
request per class (app-token auth), then sum the per-shape call deltas from
`pg_stat_statements` (or a temporary `log_statement='all'` window) and
compare against `perf-budget.yaml`. No code changes, ~5 minutes.

## What else the gate covers (future classes)

- The auth steady-state invariant: ≤ 4 statements per warm authenticated
  request (token lookup + bruteforce counts at TTL edges) — protects the
  round-3/4 caches from being reverted.
- Write classes: MKCOL, DELETE-to-trash, MOVE, chunked assembly — capture
  their floors first, then budget them.
- A pinned fixture (the bench root's 9 children + a 100-child dir) so numbers
  are comparable across runs; median-of-3, fail on median over budget.
- `--report` mode emitting the table into `docs/benchmarks.md`.

## Files touched

| File | Change |
|---|---|
| `core-rs/perf-budget.yaml` | budget table (source of truth) |
| `core-rs/crates/nc-bench/src/budget.rs` | `budget` subcommand (measure + compare + exit) |
| `core-rs/crates/nc-bench/src/main.rs` | wire the subcommand; add `serde_yaml` dep |
| `core-rs/crates/nc-difftest/src/config.rs` | `Config.db_container` (`NC_DIFFTEST_DB_CONTAINER`, default `master-database-pgsql-1`) |
| `Makefile` | `perf-gate` target |
| `core-rs/docs/benchmarks.md` | Phase 20 section with the first gate run |

## Verification

1. `make perf-gate` passes on the current code.
2. Regression proof: temporarily reintroduce one per-child query (e.g. make
   `get_props` re-query the share details per child) → the gate must fail;
   revert.
3. `cargo test --lib` unaffected (no server-crate changes).
