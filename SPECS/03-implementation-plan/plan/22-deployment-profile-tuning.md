# 22) Deployment-Profile Tuning — 2-Core, HDD, Localhost Postgres

Status: proposed. Source: deployment-profile review (2026-08-14), grounded in
the current source; line refs verified against it.

---

## Verdict

The target deployment profile inverts the optimization currency of the
round-trip work (sections 18-21): **2 cores** (`nc-server` and Postgres share
one CPU budget), an **HDD** (~8-12 ms seeks, ~100 IOPS), and Postgres on
**localhost** (~50 µs unix-socket round trips). Round trips are the abundant
resource; CPU cycles and seeks are the scarce ones. The round-trip reductions
stay correct — they are simply no longer the lever. Three consequences:

1. **The single-query CTE (section 21, Task 7) may be net-negative here.** It
   builds `json_agg`/`json_build_object` server-side (Postgres serializes to
   text) and Rust parses with serde — added CPU on both sides, traded for
   round trips worth ~50 µs each. Measured first (Wave 0); if it loses, the
   batch families (the current SQLite path) are selected by a config switch,
   never a revert — both paths exist.
2. **The sub-select gating (phase-22 22.2-C) is worth MORE here** — skipping a
   family saves real CPU, not just a round trip.
3. **Pipelining (section 21, Task 2) is definitively dead for this profile**
   — it optimizes the one resource in abundance. Recorded in phase-22
   alongside the supersession argument.

## Findings

| # | Finding | Profile cost | Location | Fix direction |
|---|---|---|---|---|
| P0 | CTE-vs-batch CPU never measured on this profile | Unknown — the decision gate | — | `pidstat`/cgroup CPU-seconds per depth-1 PROPFIND, both paths |
| P1 | Pool sized `(cores × 4).clamp(16, 64)` → **16 backends** on 2 cores; each is an OS process (~5-10 MB RSS) contending for the same CPU | Backend thrash, RSS, scheduler noise | `nc-db/src/pool.rs:148-149` | Floor to ~4-8; the pool queues instead |
| P2 | `read_dir` triple-clones every child's meta + path, plus a fresh mime `String` per child | Thousands of allocations per 1000-entry folder | `nc-dav/src/filesystem.rs:~1460-1510` | `Arc<NcMetaData>` batch map; `Arc<str>` mime names + paths |
| P3 | GET streams through `vec![0u8; count]` per chunk (alloc + memset) plus a `spawn_blocking` hop per chunk | ~1600 allocs + 100 MB zeroing + 1600 thread hops per 100 MB file | `nc-dav/src/davfile.rs:251` | Reusable `BytesMut` (uninit capacity) on `NcDavFile`; batch chunks per hop |
| P4 | `#[tokio::main]` default `max_blocking_threads = 512`; unbounded file-I/O fan-out destroys HDD elevator/NCQ ordering | Per-request latency balloons under concurrent clients | `nc-server/src/main.rs:31` | `worker_threads(2).max_blocking_threads(8)` + `Semaphore` (2-4 permits) around file I/O |
| P5 | `open()` awaits `load_meta()` then opens the file — the 10 ms seek is serialized behind a 0.05 ms query | 10 ms per open, sequential | `filesystem.rs`, `open()` ~:1700-1712 | `posix_fadvise(WILLNEED)` before the DB query; `SEQUENTIAL` on GET; `DONTNEED` after large streams |
| P6 | WebDAV GET 304 behavior unverified (vendored `handle_get`) — sync clients send `If-None-Match` constantly | A 304 avoids the entire file read | vendored `dav-server` `handle_get` | Verify; patch if it opens first |
| P7 | Preview generation (decode + resize per write) is CPU- and read-heavy; unbound it starves requests on 2 cores | Starvation under writes | `nc-preview` | 1 concurrent job; `nice`/`ionice` the drainer |
| P8 | (doc) PG tuning for HDD: `random_page_cost` default 4.0 is **correct** — keep SSD-tuned configs (1.1) out; `effective_cache_size` to real RAM; modest `shared_buffers` (let the OS page cache work); `commit_delay`/`commit_siblings` group commit (a WAL fsync is a full rotation); throttle autovacuum; `max_connections` aligned with P1 | Planner and WAL behaviour on HDD | deployment doc | Section only; no code |
| P9 | (doc) Compression: PROPFIND XML ~10:1, but gzip is expensive on a CPU-starved box | CPU | deployment doc | Skip for LAN-local; zstd/gzip level 1 (not 6) if remote |

## Sequencing

```
Wave 0  Measure the CTE (decision gate)         ── settles P0
Wave 1  CPU & I/O discipline: P1, P4, P2, P3    ── the contained wins, one commit each
Wave 2  Seek & cache discipline: P5             ── overlaps 10 ms with 0.05 ms
Wave 3  Verification: P6                        ── highest-leverage check on HDD
Wave 4  Resource governance: P7                 ── sits with Wave 1's I/O discipline
Docs    P8, P9, and the pipelining record (phase-22)
```

Waves 1 and 3 are independent; within Wave 1, P1 and P4 are the two
one-commit changes and block nothing.

---

## Wave 0 — Measure the CTE (decision gate)

Total CPU-seconds per depth-1 PROPFIND (`nc-server` + `postgres` combined,
via `pidstat` or cgroup accounting) on the target profile, CTE path vs the
batch-families path (the SQLite arm's shape run against Postgres).

- If the CTE loses: add a config switch (`propfind.backend = cte | batch`)
  selecting the existing batch families on Postgres; the CTE stays the
  default for the dev/remote profiles. Behavior-neutral either way — same
  bytes, the milestone suite is the parity gate.
- If it wins: close with the numbers.

**Gate:** the measurement table recorded in `docs/benchmarks.md`; the switch
(if added) passes diff-test + perf-gate for the default profile.

## Wave 1 — CPU & I/O discipline

### P1 — Pool floor (`nc-db/src/pool.rs:148-149`)

`(cores * 4).clamp(16, 64)` yields 16 backends on 2 physical cores. Each
Postgres connection is a separate OS process (~5-10 MB RSS) contending for
the same 2 cores. Target ~4-8 on small boxes: lower the clamp floor (or make
the ceiling configurable) so 2 cores → 8; `min_connections(5)` stays.

**Gate:** diff-test + perf-gate unchanged (no statement-count change); bench
p50 under concurrent load improves on the target profile.

### P4 — Bound concurrent disk I/O (`nc-server/src/main.rs:31`)

`#[tokio::main]` defaults to `max_blocking_threads = 512`. On an HDD, 512
concurrent seeks destroy elevator/NCQ ordering — limiting I/O concurrency
*raises* throughput on rotational media. Replace with
`Builder::new_multi_thread().worker_threads(2).max_blocking_threads(8)` and a
`Semaphore` (permits ≈ 2-4) around actual file I/O so queue depth stays sane
under concurrent clients.

**Gate:** `make bench-load` on the target — throughput up, p99 down.

### P2 — `read_dir` allocations (`nc-dav/src/filesystem.rs:~1460-1510`)

Every child's metadata and path is cloned three times:

```rust
metas.push((key, meta.clone()));                    // 1
entries.push(Ok(Box::new(NcDirEntry { meta })));    // moves the original
…
batch_inner.meta.insert(key.clone(), meta.clone()); // 2 + 3
let child_paths: Vec<String> = metas.iter().map(|(k, _)| k.clone()).collect();
```

Plus `cache.get_name(child.mimetype)…to_string()` allocates a fresh mime
`String` per child. Fix: `Arc<NcMetaData>` in the batch map, `Arc<str>` for
mime names and paths; `NcDirEntry` keeps the owned meta (moved once).

**Gate:** unit tests; diff-test byte parity; bench flat-or-better.

### P3 — GET streaming buffer (`nc-dav/src/davfile.rs:251`)

`let mut buf = vec![0u8; count];` allocates and zeroes per chunk, plus a
`spawn_blocking` hop per chunk. Fix: a reusable buffer on `NcDavFile`
(`BytesMut` with uninit capacity), reading into it; batch chunks per blocking
hop.

**Gate:** GET/read scenarios byte-identical; buffer memory bounded across
chunks.

## Wave 2 — Seek & cache discipline (P5)

`open()` awaits `load_meta()` and then opens the file — sequential, though
independent. Issue `posix_fadvise(POSIX_FADV_WILLNEED)` on the file as soon as
the path is known so the platter seek overlaps the DB query (10 ms overlapped
with 0.05 ms — the only parallelism that pays here). Also
`POSIX_FADV_SEQUENTIAL` on GET (bigger kernel readahead) and
`POSIX_FADV_DONTNEED` after streaming a large file — a single big download
must not evict Postgres's page cache, which on a low-RAM box is what keeps
index reads off the platter.

**Gate:** bench p50; repeat-read cache behavior sanity-checked.

## Wave 3 — Verify If-None-Match on GET (P6)

Previews handle 304 (`response.rs:63-124`); the WebDAV GET path (vendored
`dav-server`'s `handle_get`) could not be confirmed to short-circuit before
opening the file. Verify the vendored handler; patch it if it opens first.

**Gate:** a 304 scenario (`If-None-Match` with the current etag) issues zero
file reads.

## Wave 4 — Preview generation governance (P7)

Every write queues into `oc_preview_generation` (CLAUDE.md); image
decode + resize is CPU- and read-heavy — on 2 cores it will starve request
handling. Bound the generator to **1 concurrent job**, and run the drainer
under `nice`/`ionice` so interactive requests win.

**Gate:** write-path scenarios unchanged; CPU shares sane under concurrent
writes + previews.

## Deployment documentation (P8, P9)

- PG tunables (P8): `random_page_cost = 4.0` (HDD-correct default — guard
  against SSD configs leaking in), `effective_cache_size` sized to real RAM,
  modest `shared_buffers` (the OS page cache does the work on low-RAM),
  `commit_delay`/`commit_siblings` group commit (safe, unlike
  `synchronous_commit = off`), autovacuum cost throttling, `max_connections`
  aligned with P1.
- Compression decision (P9): probe what the server compresses today; skip for
  LAN-local clients; zstd level 1 (or gzip level 1 — not 6) if remote.
- The pipelining record: one phase-22 Changes line — pipelining optimizes the
  abundant resource (round trips) on this profile, dead alongside the
  supersession argument.

**Gate:** doc sections; no code change.

## Out of scope

| Item | Why not |
|---|---|
| Reverting the single-query CTE | The config switch (Wave 0), never the revert |
| Pipelining (section 21, Task 2) | Definitively dead for this profile; recorded in phase-22 |
| Anything beyond Postgres + SQLite | CLAUDE.md principle 6 |

## Exit criteria

1. Wave 0 measurement recorded; the CTE decision made; if the switch was
   added, both profiles pass the milestone suite.
2. Pool on the 2-core box = 4-8 backends; `bench-load` p50/p99 improved.
3. `read_dir` per-request allocations reduced by the expected order (or the
   bench shows it); diff-test byte parity holds.
4. A 100 MB GET streams without per-chunk allocation; buffer memory bounded.
5. `open()` issues `WILLNEED` before `load_meta`; bench p50 improved.
6. `If-None-Match` 304 issues zero file reads (live probe).
7. Preview generation capped at 1 job; write-path scenarios unchanged.
8. Deployment doc carries the PG tunables, the compression decision, and the
   pipelining record.
