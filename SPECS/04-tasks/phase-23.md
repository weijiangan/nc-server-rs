# Phase 23 — Deployment-profile tuning (2-core, HDD, localhost Postgres)

Goal: execute plan 22's waves on the target profile — the CTE-vs-batch CPU
decision gate first (Wave 0), then the CPU & I/O discipline (pool floor,
bounded I/O concurrency, `read_dir` allocations, GET streaming buffer), the
seek/cache overlap, the `If-None-Match` verification, preview-generation
governance, and the deployment documentation. Behavior-neutral items carry
the standard dev-stack gates (diff-test, perf-gate, unit tests); the
profile-specific wins are measured on the target box and recorded in
`docs/benchmarks.md` — not the dev-docker numbers.

Full plan: [`SPECS/03-implementation-plan/plan/22-deployment-profile-tuning.md`](../03-implementation-plan/plan/22-deployment-profile-tuning.md).

---

## Verifiable stops

| Stop | Tasks | Gate |
|---|---|---|
| S0 — Wave 0: CTE decision | 23.1 | CPU measurement recorded; the switch (if any) passes the milestone suite for the default profile |
| S1 — Wave 1: CPU & I/O discipline | 23.2-23.5 | `cargo test --lib`; diff-test + perf-gate unchanged; profile bench where applicable |
| S2 — Wave 2: seek overlap | 23.6 | bench p50 on the target; repeat-read cache sanity |
| S3 — Wave 3: 304 verify | 23.7 | `If-None-Match` 304 issues zero file reads |
| S4 — Wave 4: previews | 23.8 | write-path scenarios unchanged; CPU shares sane |
| S5 — Docs | 23.9 | deployment doc sections; pipelining record in phase-22 |

---

## Wave 0 — The CTE decision gate

### 23.1 Measure CTE vs batch CPU (plan P0)

- [x] Measure total CPU-seconds per depth-1 PROPFIND (`nc-server` + `postgres` combined, via `pidstat` or cgroup accounting) on the target profile — CTE path vs the batch-families path run against Postgres; record in `docs/benchmarks.md`.
- [ ] If the CTE loses: the `propfind.backend = cte | batch` config switch (plan 22, Wave 0) selecting the existing batch families on Postgres; the CTE stays the default elsewhere; behavior-neutral (same bytes) — the milestone suite is the parity gate for both profiles.

**Verify:** the measurement table in `docs/benchmarks.md`; the decision (keep CTE / add switch) recorded in the Changes log.

## Wave 1 — CPU & I/O discipline

### 23.2 Pool floor (plan P1, `nc-db/src/pool.rs:148-149`)

- [x] Lower the `max_connections` clamp floor so 2 physical cores → 8 (or make the ceiling configurable); `min_connections(5)` stays.

**Verify:** perf-gate + diff-test unchanged (no statement-count change); the pool reports 4-8 backends on the target.

### 23.3 Bound concurrent disk I/O (plan P4, `nc-server/src/main.rs:31`)

- [x] Replace `#[tokio::main]` with `Builder::new_multi_thread().worker_threads(2).max_blocking_threads(8)`; add a `Semaphore` (permits ≈ 2-4) around actual file I/O so queue depth stays sane under concurrent clients.

**Verify:** `make bench-load` on the target — throughput up, p99 down.

### 23.4 `read_dir` allocations (plan P2, `nc-dav/src/filesystem.rs:~1460-1510`)

- [x] `Arc<NcMetaData>` in the batch map; `Arc<str>` for mime names and paths; kill the per-child triple clone (mime `to_string` included). `NcDirEntry` keeps the owned meta.

**Verify:** unit tests; diff-test byte parity; bench flat-or-better.

### 23.5 GET streaming buffer (plan P3, `nc-dav/src/davfile.rs:251`)

- [x] A reusable buffer on `NcDavFile` (`BytesMut` with uninit capacity), reading into it; batch chunks per `spawn_blocking` hop.

**Verify:** GET/read scenarios byte-identical; buffer memory bounded across chunks.

## Wave 2 — Seek & cache discipline

### 23.6 Overlap the seek with the DB work (plan P5, `open()` ~:1700-1712)

- [x] `posix_fadvise(POSIX_FADV_WILLNEED)` on the file as soon as the path is known — before `load_meta` — so the platter seek overlaps the DB query; `POSIX_FADV_SEQUENTIAL` on GET; `POSIX_FADV_DONTNEED` after streaming a large file.

**Verify:** bench p50 on the target; repeat-read cache behavior sanity-checked (a large download does not evict Postgres's page cache).

## Wave 3 — Verification

### 23.7 Verify `If-None-Match` on GET (plan P6, vendored `dav-server` `handle_get`)

- [x] Verify the vendored handler short-circuits on `If-None-Match` before opening the file; patch it if it opens first (previews already 304 correctly, `response.rs:63-124`).

**Verify:** a 304 scenario (`If-None-Match` with the current etag) issues zero file reads.

## Wave 4 — Preview governance

### 23.8 Bound preview generation (plan P7, `nc-preview`)

- [x] Bound the generator to 1 concurrent job; run the drainer under `nice`/`ionice` so interactive requests win.

**Verify:** write-path scenarios unchanged; CPU shares sane under concurrent writes + previews.

Note: the bound already exists — `preview_concurrency_new` (config) sizes the
generation semaphore (`nc-preview/src/concurrency.rs:65`, held per generation
in `nc-server/src/preview_gen.rs`), and there is no separate Rust drainer
process to `nice` (the drainer is PHP's cron `PreviewJob` — an ops concern).
No code change needed; the deployment doc (23.9) recommends
`preview_concurrency_new = 1` for this profile.

## Deployment documentation

### 23.9 Deployment doc (plans P8, P9 + the pipelining record)

- [x] Postgres tunables section: `random_page_cost = 4.0` (HDD-correct — keep SSD configs out), `effective_cache_size` to real RAM, modest `shared_buffers`, `commit_delay`/`commit_siblings` group commit, autovacuum cost throttling, `max_connections` aligned with 23.2.
- [x] Compression decision: probe what the server compresses today; skip for LAN-local clients; zstd level 1 (or gzip level 1 — not 6) if remote.
- [x] One phase-22 Changes line: pipelining is definitively dead for this profile — it optimizes the abundant resource (round trips), alongside the supersession argument.

**Verify:** the doc sections; the phase-22 entry.

---

## Deviations from the task descriptions

(none yet)

## Changes

Execution history only: what was tried, reverted, and why; root causes and
verification results not already stated in the task text. Nothing that merely
restates a task or the code.

- 2026-08-14: **Milestone run (fresh `down -v`, fixed binary): suite 20/20,
  perf-gate green, first-baseline comparison recorded.** Suite: 18 passed
  first pass; 01 and 15 re-ran clean — the documented first-run artifact
  class (lazy cache-row + storage-root bump; one-sided vcategory rows).  The
  bench comparison vs the 2026-08-10 baseline is in `docs/benchmarks.md`:
  depth-1 PROPFIND 11.9 → 6.0 ms (−50%), depth-1 load probe 750 → 1826 req/s
  (+143%), ratios improved in 18/20 scenarios; the oracle measured ~2× slower
  than baseline across the board (box CPU contention) so ratios are the
  cross-run metric.  One harness finding en route: `make bench` scenario 30
  flaked with an empty-body 429 — bench replays do not run the scenario
  cleanup, so leftover `oc_share` rows + accumulated `oc_bruteforce_attempts`
  trip PHP's login throttler on the proxied share_create; the fix is the
  documented brute-force reset (delete both tables) before re-running.
- 2026-08-14: **Milestone gate: propfind_depth1 budget corrected 12 → 13 (delta 1 → 2).** The perf-gate re-run on the fixed binary breached the 22.2-C budget with a steady [13,13,13] across two independent runs — and the 13th statement is real, not a regression: zero PROPFIND-path code changed between the T8.1 fix (ea1dcd5) and HEAD (verified by git diff), and the depth-1 set is exactly depth-0's 11 + CTE + custom-props batch. The 12 budget was set during the 2026-08-14 milestone on the T8.1 desync-buggy binary — a depth-1 request killed by the accounts-fallback panic produced truncated execute counts, and the gate's probe does not validate the response status (budget.rs). Corrected in perf-budget.yaml per the no-headroom policy; gate green.
- 2026-08-14: **23.8 resolved without code — the bound already existed.** The
  `preview_concurrency_new` config key sizes the generation semaphore
  (phase 11.3), held per generation in `preview_gen.rs`; there is no Rust
  drainer process to run under `nice`/`ionice` (PHP's cron `PreviewJob` is
  the drainer). Operator-confirmed.  The deployment doc now recommends
  `preview_concurrency_new = 1` for this profile.
- 2026-08-14: **23.9 done — deployment doc section + phase-22 record.** New
  "Target-profile tuning" section in `core-rs/docs/deployment.md`: PG
  tunables (random_page_cost 4.0, effective_cache_size to real RAM, modest
  shared_buffers, commit_delay/siblings group commit, autovacuum throttling,
  max_connections 40 — aligned with the 23.2 pool clamp `(cores*4).clamp(4,64)`,
  verified in `pool.rs`), the page-cache discipline rules that the 23.6
  fadvise hints assume, and the compression decision (probed: nc-server sends
  no Content-Encoding today — no middleware compression; skip for LAN, zstd
  level 1 / gzip level 1 on the reverse proxy for HTML/JS only if remote).
  One phase-22 Changes line records the pipelining verdict for this profile.
- 2026-08-14: **23.7 verified + patched — `handle_get` opened the file before
  resolving conditional headers.**  The vendored handler opened at
  `fs.open()` and only then ran `conditional::if_match`, so a 304 request
  paid a disk open + `load_meta`.  The check now runs against the pre-open
  `fs.metadata()` result — the same cache row the opened file reports, so the
  etag/last-modified comparison is identical — and the early return
  replicates the old 304/412 header set (ETag, Last-Modified, Accept-Ranges,
  Content-Type, Content-Length: full size for 304, 0 for 412).  The 416
  `no_body` path is untouched; `redirect_url` defaults to `None` in nc-dav,
  so the redirect-before-conditional ordering is a non-issue.  Zero file
  reads on 304 (metadata DB query only) — live probe pending.
- 2026-08-14: **23.6 implemented — seek overlap + page-cache discipline.**
  Read-only `open()` now opens the file, issues `WILLNEED` + `SEQUENTIAL`,
  and only then runs `load_meta`, so the platter seek overlaps the DB query.
  `DONTNEED` fires on the last chunk of any ≥32 MiB stream — keyed on
  `streamed >= meta.size`, because `handle_get` reads exactly `len` bytes and
  `read` never returns 0 on a completed GET (an EOF hook would never fire).
  One dependency wrinkle: rustix's `fs::fadvise` is unusable from outside
  rustix — its `Advice` parameter type lives in a private module (still true
  on rustix main), so nc-dav carries a 10-line `libc::posix_fadvise` shim and
  the crate root was relaxed `forbid(unsafe_code)` → `deny(unsafe_code)`,
  the single documented block in `fadvise.rs` (user-approved).  Bench p50 +
  repeat-read cache sanity on the target pending.
- 2026-08-14: **23.2-23.5 implemented (Wave 1).** Pool floor 16 → 4 (2-core
  → 8 backends); bounded runtime (2 workers / 8 blocking threads) with the
  davfile/filesystem blocking helpers on `spawn_blocking` and a shared
  4-permit file-I/O semaphore; read_dir allocations collapsed to one
  `Arc<NcMetaData>` per child with `Arc<str>` mime strings from the cache;
  the GET read buffer is a reused `BytesMut` (capacity persists across
  chunks — the zero-fill stays, `forbid(unsafe_code)`). One 23.3-introduced
  bug fixed en route: a duplicated `file.take()` in `read_bytes` that would
  have failed every read (no unit test covered it). Verified: 546 lib tests
  green; `cargo test --lib`; the profile gates (bench-load on the target,
  diff-test) pending the milestone run.
- 2026-08-14: **23.1 measured — decision B (keep the CTE).** The 200-child
  allprop A/B on the fixed binary: CTE 126.2 ms/req combined CPU
  (nc-server 123.1 + postgres 3.10) vs batch families 119.9
  (117.6 + 2.37) — the CTE's json_agg serialization + serde parse costs
  ~6.3 ms/req over the seven batch statements, which is ~5%, under the 10%
  switch threshold. Recorded in `docs/benchmarks.md`; the
  `propfind.backend = cte | batch` switch stays a documented option.
  Method notes: the earlier arms were invalidated twice — the first by the
  pre-fix binary (the cached_sql desync and the NULL-decode panics, which
  also corrupt the 2026-08-14 milestone's propfind scenarios), the second by
  Nextcloud's cron `files:cleanup` job sweeping the DB-only test directory
  (the A/B now seeds real disk files). The NC_FORCE_BATCH instrument was
  temporary and reverted after the measurement.

