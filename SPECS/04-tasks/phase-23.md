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

- [ ] `Arc<NcMetaData>` in the batch map; `Arc<str>` for mime names and paths; kill the per-child triple clone (mime `to_string` included). `NcDirEntry` keeps the owned meta.

**Verify:** unit tests; diff-test byte parity; bench flat-or-better.

### 23.5 GET streaming buffer (plan P3, `nc-dav/src/davfile.rs:251`)

- [ ] A reusable buffer on `NcDavFile` (`BytesMut` with uninit capacity), reading into it; batch chunks per `spawn_blocking` hop.

**Verify:** GET/read scenarios byte-identical; buffer memory bounded across chunks.

## Wave 2 — Seek & cache discipline

### 23.6 Overlap the seek with the DB work (plan P5, `open()` ~:1700-1712)

- [ ] `posix_fadvise(POSIX_FADV_WILLNEED)` on the file as soon as the path is known — before `load_meta` — so the platter seek overlaps the DB query; `POSIX_FADV_SEQUENTIAL` on GET; `POSIX_FADV_DONTNEED` after streaming a large file.

**Verify:** bench p50 on the target; repeat-read cache behavior sanity-checked (a large download does not evict Postgres's page cache).

## Wave 3 — Verification

### 23.7 Verify `If-None-Match` on GET (plan P6, vendored `dav-server` `handle_get`)

- [ ] Verify the vendored handler short-circuits on `If-None-Match` before opening the file; patch it if it opens first (previews already 304 correctly, `response.rs:63-124`).

**Verify:** a 304 scenario (`If-None-Match` with the current etag) issues zero file reads.

## Wave 4 — Preview governance

### 23.8 Bound preview generation (plan P7, `nc-preview`)

- [ ] Bound the generator to 1 concurrent job; run the drainer under `nice`/`ionice` so interactive requests win.

**Verify:** write-path scenarios unchanged; CPU shares sane under concurrent writes + previews.

## Deployment documentation

### 23.9 Deployment doc (plans P8, P9 + the pipelining record)

- [ ] Postgres tunables section: `random_page_cost = 4.0` (HDD-correct — keep SSD configs out), `effective_cache_size` to real RAM, modest `shared_buffers`, `commit_delay`/`commit_siblings` group commit, autovacuum cost throttling, `max_connections` aligned with 23.2.
- [ ] Compression decision: probe what the server compresses today; skip for LAN-local clients; zstd level 1 (or gzip level 1 — not 6) if remote.
- [ ] One phase-22 Changes line: pipelining is definitively dead for this profile — it optimizes the abundant resource (round trips), alongside the supersession argument.

**Verify:** the doc sections; the phase-22 entry.

---

## Deviations from the task descriptions

(none yet)

## Changes

Execution history only: what was tried, reverted, and why; root causes and
verification results not already stated in the task text. Nothing that merely
restates a task or the code.

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

