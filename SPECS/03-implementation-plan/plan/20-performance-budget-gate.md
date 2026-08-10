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

## Budgets (measured floors, app-token auth, warm caches, Postgres statement log)

Source of truth: `core-rs/perf-budget.yaml`. Budgets = 2× measured floor.

| class | request | measured | budget |
|---|---|---|---|
| status | `GET /status.php` | 0-2 | 5 |
| get_file | `GET /remote.php/webdav/hello.txt` | ~8 | 12 |
| propfind_depth0 | `PROPFIND /remote.php/webdav/` depth 0 | ~12-15 | 20 |
| propfind_depth1 | `PROPFIND /remote.php/webdav/` depth 1 | ~19-26 | 30 |
| put_new | `PUT /remote.php/webdav/q-budget-<ts>.txt` | ~23 | 30 |
| **scaling delta** | depth1 − depth0 | ~8 | **10** |

The scaling delta is the regression detector: the batch queries are fixed-cost
regardless of N, so any per-child query reintroduction shows up as a linear
slope.

## Measurement mechanism (Phase 1)

`nc-bench` gains a `budget` subcommand (black-box like the rest of the harness —
links no `nc-*` server crate):

1. Load `perf-budget.yaml`.
2. Create an app token on the SUT (existing `auth::create_token`).
3. Enable `log_statement='all'` on the SUT's Postgres (superuser DSN).
4. Per class: one unmeasured warmup request → open a counting window →
   run the probe once → count `execute sqlx` lines in the SUT container's
   Postgres log since the window start.
   - The `execute sqlx` filter counts only Rust's prepared statements;
     PHP/Doctrine logs as `<unnamed>` and background cron is excluded by
     construction.
5. PUT uses a unique path per run and cleans it up after the window.
6. `depth1 − depth0` scaling-delta check.
7. Disable logging; print the table; exit non-zero on any breach.

Gate wiring: `make perf-gate` (prerequisites: `make diff-up`-style stack up).

## Phase 2 (follow-up, not in this phase)

An in-process per-request query counter (task-local + counting `Executor`
wrapper around the pool) exposed as a per-request trace and an optional
`NC_QUERY_BUDGET` env that logs a warning when a request class exceeds its
budget — the same enforcement on deployments where the Postgres log is not
accessible (e.g. the HDD-RAID production target, where each round trip is
expensive). The budget file is shared.

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
