# 19. Performance improvements from the Phase 17 flamegraphs

## Context

Phase 17's first flamegraph pass (under load) identified three addressable costs, ranked by leverage:

1. **Per-request service-stack cloning** — the largest non-skeleton cost under load: `Route::clone` / `CloneService::clone_box` / `MapFuture/MapErr/MapIntoResponse::clone` + their `drop_in_place` out-sample all handler frames. The router is ~70-80 routes; ~30 of them are near-duplicate DAV mount registrations (`nc-server/src/router.rs:158-209`) that a single classified wildcard can replace.
2. **`try_static_files` fs stat on every request** — every GET/HEAD stats `nc_root.join(path)` + builds `ServeDir` before routing, though static files live only under `/core/ /dist/ /themes/ /apps/` plus two exact root files.
3. **Two bruteforce COUNT queries per request** (`nc-auth/src/bruteforce.rs`) — ~3-4% of request CPU at light load.

All three are verified against the running stack (webroot ground truth confirmed: static roots + `robots.txt` + `index.html` exist; repo files like `AUTHORS`/`README.md` are currently served statically — blocking them is a fidelity-neutral security gain since real nginx installs deny them).

## Task 1 — Static-file path whitelist (`nc-server/src/router.rs:36-59`)

In `try_static_files`, before the `tokio::fs::metadata` call, require the path to start with one of `/core/`, `/dist/`, `/themes/`, `/apps/` **or** be exactly `/robots.txt` / `/index.html`. Everything else (all API traffic: status.php, ocs, dav, index.php, well-known…) skips the stat and falls straight through. Keep the existing `.php` and `..` checks.

- Behavior change: `/AUTHORS`, `/README.md`, `/3rdparty/*`, dotfiles are no longer served statically (they fall through to routing → 404). Deliberate; document in phase-18 Changes log.
- The `index.html` inclusion preserves the dev image's installing page (GET /index.html); GET / still falls through as today.

## Task 2 — DAV route consolidation (`nc-server/src/router.rs`)

Replace the ~30 DAV mount routes with 6:

```rust
.route("/remote.php/dav", any(dav_arbiter_handler))
.route("/remote.php/dav/", any(dav_arbiter_handler))
.route("/remote.php/dav/{*path}", any(dav_arbiter_handler))
.route("/dav", any(dav_arbiter_handler))
.route("/dav/", any(dav_arbiter_handler))
.route("/dav/{*path}", any(dav_arbiter_handler))
```

Extend `dav_arbiter_handler` (router.rs:61-81, which already intercepts SEARCH/REPORT and falls through to `nc_dav::dav_handler`) to classify by the path remainder after the mount root (`/remote.php/dav` or `/dav`):

1. `SEARCH`/`REPORT` → FastCGI proxy (unchanged)
2. remainder starts with `/versions /comments /trashbin /principals /calendars /public-calendars /system-calendars /addressbooks /avatars /access-control` → FastCGI proxy (was 11×2 explicit routes)
3. remainder starts with `/uploads` → `nc_dav::upload_handler` (was 2 routes)
4. remainder == `/bulk` AND method == POST → `nc_dav::bulk_handler` (was 2 routes); non-POST /bulk now falls to `dav_handler` → 404 — actually *more* PHP-faithful than the current axum 405 (sabreDAV treats "bulk" as an ordinary resource path)
5. everything else → `dav_handler` (unchanged fall-through)

Subtree classification uses `starts_with`, so bare `/remote.php/dav/versions` (no trailing content) now proxies too — today it falls into `dav_handler` and 404s; PHP would route it into the versions tree. More faithful; the diff suite doesn't cover bare subtree paths.

Keep the registry-built per-app routes untouched (their explicit 404 semantics for unknown paths is a deliberate Phase 7.5 decision — documented in router.rs comments).

## Task 3 — Throttler COUNT cache (`nc-auth/src/bruteforce.rs`)

`check_throttle` runs two `COUNT(*)` queries (short 30-min window, long 12-h window) per request. Cache the counts per `(action, subnet)` in a small DashMap with a 1-2 s TTL (nc-auth already uses DashMap for the token cache). The count only changes when `record_attempt` inserts a row; a ≤2 s staleness on the delay/429 decision is immaterial (delay formula is exponential). Keep the DB as source of truth on miss.

## Files touched

| File | Change |
|---|---|
| `core-rs/crates/nc-server/src/router.rs` | whitelist in `try_static_files`; DAV route block → 6 routes; `dav_arbiter_handler` classification |
| `core-rs/crates/nc-auth/src/bruteforce.rs` | count cache (DashMap + TTL) in `check_throttle` |
| `SPECS/04-tasks/phase-18.md` | new phase doc: tasks + gates + Changes log (whitelist semantics, /bulk 405→404 fidelity note) |
| `core-rs/docs/benchmarks.md` | refresh baseline numbers + note the deltas |

## Verification

1. `cargo check` + `cargo test --lib` in `core-rs`.
2. **Static parity probes**: `GET /core/img/logo.svg`, `/dist/*`, `/apps/files/img/icon.svg`, `/themes/*`, `/robots.txt`, `/index.html` → 200; `GET /AUTHORS` → 404 (documented change).
3. **`make diff-test`** — the full 20-scenario differential suite must stay green (webdav/upload/bulk/share flows all traverse the arbiter or the files tree).
4. **Before/after measurement**: `make bench-one SC=01_propfind_readonly` + `make bench-load` + a fresh `make profile` flamegraph — compare clone-machinery share against `profile-1786300646.svg`; record the delta in benchmarks.md.

---

## Round 2 — from the Phase 18 flamegraphs (2026-08-10)

The Phase 18 re-profile (`profiles/profile-1786308169.svg`, `profile-1786310260.svg`) confirmed the round-1 wins in data (clone machinery 15.8% → 1.8% of self CPU; alloc/drop 13% → ~2.5%) and surfaced the next three addressable costs, ranked:

1. **~30% of self CPU is per-request future-wrapper machinery** — `Pin::poll` → `MapIntoResponseFuture` → `RouteFuture` → `Oneshot` → `catch_unwind` → `TryFuture` → `FromFn::ResponseFuture` → `Next::run` — the per-await poll chain of a deep tower stack. With three `from_fn` middleware layers on the DAV routes (`router.rs:284-291`), every request wakes through ~8 wrapper polls per await.
2. **~3.7-3.9% logging/tracing** (`tracing::instrument::Instrumented::poll` ~1.2% + thread-local access ~1.8% + span writes) — per-request spans at debug.
3. **Depth-1 PROPFIND per-child property lookups** — ~11 queries per child (~124 per request on the 9-child root). A *latency* cost invisible to CPU flame graphs. **DONE — Phase 18.1** (2026-08-10): per-request `PropfindBatch` + 7 batched queries; ~15 statements per request, zero per-child queries. Detail: phase-18 Changes log and `docs/benchmarks.md` §Phase 18.1.

Caveat that shapes the next round: the round-2 dumps captured only 256-290 samples over 10 s (~0.3 s of CPU) — the SUT is I/O-bound on the shared Postgres (duty cycle <1%), so the flame graph is dominated by await-side scaffolding and anything below ~1% is ±1-sample noise. All three numbers above are the trustworthy part.

### Task 4 — Consolidate the `from_fn` middleware layers (`nc-server/src/router.rs:284-291`)

The DAV routes carry three `from_fn_with_state` layers (auth, maintenance, throttler). Each layer adds ~2 wrapper polls per await (`FromFn::call` + `ResponseFuture::poll` + `Next::run::closure`) plus `MapErr`/`MapIntoResponse` per hop — the `from_fn`-attributable share of the ~30% wrapper machinery is ~3-4% of self CPU in the round-2 flamegraphs (`ResponseFuture::poll` 1.39% + `Next::run` 1.31-1.34% + `FromFn::call` 1.39%, each ~3-4 samples).

Merge the three layers into one composite `from_fn` middleware that runs the same checks in the same order (auth → maintenance → throttler) inside a single future, preserving each check's response byte-for-byte.

Expected: ~2.5-3% of CPU when CPU-bound — a **headroom play, not a p50 win** at current load levels (the SUT is I/O-bound; the per-request CPU share is ~10-20 μs of the ~2-4 ms p50, so the wall clock is noise-bound). It converts to throughput only at CPU saturation.

### Task 5 — Trim per-request span/logging overhead

First establish what the profiling runs actually log: the SUT inherits `RUST_LOG: debug` from compose (`nextcloud-docker-dev/docker-compose.yml`), so the measured 3.7-3.9% logging is largely measurement artifact. Then:

- Gate the per-request spans (auth, DAV handler, FastCGI proxy, OCS dispatch) behind a static level check so they cost ~nothing at `info`.
- Move the per-request spans to `trace` (they are the excimer/xhprof counterpart for debugging, not operational logs).
- Re-profile with `RUST_LOG=info` to measure the true residual (expect the 3.7-3.9% to mostly disappear; the residual `Instrumented::poll` + `LocalKey` cost is the real target).

### Task 6 — (done) Depth-1 PROPFIND batching

Round-1 cost-center follow-up ("batch per-child property lookups") implemented as Phase 18.1 — see the phase-18 Changes log and `docs/benchmarks.md` §Phase 18.1. Kept here so the round-2 list is complete.

### Files touched (round 2)

| File | Change |
|---|---|
| `core-rs/crates/nc-dav/src/row.rs` | 7 `*_batch` query helpers + 6 batch-vs-single unit tests |
| `core-rs/crates/nc-dav/src/filesystem.rs` | per-request `PropfindBatch`; `read_dir` population; `load_meta`/`get_props` cache reads |
| `core-rs/docs/benchmarks.md` | Phase 18.1 section |
| `SPECS/04-tasks/phase-18.md` | Changes log entry (18.1) + follow-up section |

### Verification (round 2)

1. `cargo test --lib` — nc-dav 311 tests incl. batch-vs-single consistency.
2. `make diff-test` — 20/20 scenarios (documented first-run `home-root-size`/replay-noise divergences clear on rerun).
3. Explicit-body depth-1 PROPFIND byte-compared against PHP on the same DB — per-path blocks identical modulo the pre-existing etag quote-escaping.
4. Postgres statement log: ~124 → ~15 statements per depth-1 request; zero per-child queries.
5. **Re-profile discipline for round 3**: keep load peaking at SIGUSR2 time (the `make profile` flow signals 3 s in — the round-2 dumps caught mostly quiescence), raise concurrency until the SUT saturates CPU, run with `RUST_LOG=info`. Otherwise the next dump will again be ~90% idle scaffolding.
