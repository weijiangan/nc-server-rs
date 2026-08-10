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

---

## Round 3 — query-level opportunities (2026-08-10)

Code-inspection findings (not flame-graph-driven — query counts are invisible to CPU sampling). Verified against the sources; ranked by leverage. Net effect: depth-1 PROPFIND ~15 → ~11 statements, depth-0 ~12 → ~9, GET ~5 → ~2, plus one query removed from every new-path write.

### Task 7 — (done) Remove the hidden miss-path query + per-lookup info logging (`nc-dav/src/row.rs:124-175`)

`lookup_by_path` carries two unaccounted costs:

- **Every miss runs an extra debug query.** The `Ok(None)` branch unconditionally executes the "Debug: query without the storage filter" fallback (`SELECT fileid, storage, path … WHERE path_hash = $1`). Every PUT of a *new* file misses (the write-path existence check), so every file creation pays 2 queries instead of 1. Fix: drop the fallback or gate it behind `tracing::enabled!(Level::TRACE)`.
- **`tracing::info!("lookup_by_path: found")`** fires on every successful lookup with `%path`/`%hash` formatting — live in production logs at `info`. Downgrade to `trace!` (part of the measured 3.7-3.9% logging).

### Task 8 — (done) Cache the 2FA-provider + admin-group checks (`nc-server/src/middleware/auth.rs`, `nc-auth/src/lib.rs`)

Every authenticated request runs two raw queries — `SELECT COUNT(*) FROM oc_twofactor_providers WHERE uid = $1 AND enabled = 1` and `SELECT uid FROM oc_group_user WHERE gid = 'admin' AND uid = $1` — on top of the (already cached) token lookup. Phase 18.3 cached only the bruteforce counts; this is the rest of the benchmarks.md "auth_layer per-request DB work" cost-center entry. Cache both per `(uid)` in the nc-auth DashMap with a 30-60 s TTL (2FA enablement / group membership change rarely; staleness immaterial). Removes 2 queries from every authenticated request on every endpoint.

### Task 9 — (done) Join `oc_filecache_extended` into the row queries (match PHP)

PHP's `getFolderContentsById` (`lib/private/Files/Cache/Cache.php:214`) fetches children + metadata in **one** query (`selectFileCache` + `selectMetadata` LEFT JOIN). Rust currently issues `list_children` + `list_extended_batch` (2 queries per `read_dir`) and `lookup_by_path` + `get_extended` (2 per `load_meta`). A LEFT JOIN with COALESCE defaults (absent extended row = zero times, the current fallback semantics exactly) collapses both pairs into single queries. Saves 1-2 queries per PROPFIND and per GET.

### Task 10 — (done) `load_meta` store-on-miss (`nc-dav/src/filesystem.rs`)

The PropfindBatch meta map is populated only by `read_dir` (the defensive invariant); `load_meta` consults but never stores. All three callers are read-only — `metadata()` (:1333), `open()` read path (:1560), `get_props` (:2457) — the write path calls `lookup_by_path` directly. Storing on miss is verifiably safe and kills the root's **double lookup** per PROPFIND (`fs.metadata(root)` then `get_props(root)` → `load_meta(root)` — two queries for the same row), on depth-0 and depth-1 alike.

### Files touched (round 3)

| File | Change |
|---|---|
| `core-rs/crates/nc-dav/src/row.rs` | miss-path debug query gated/removed; `trace!` instead of `info!`; optional extended JOIN variants of `lookup_by_path`/`list_children` |
| `core-rs/crates/nc-auth/src/lib.rs` | 2FA-provider + admin DashMap cache (TTL) |
| `core-rs/crates/nc-server/src/middleware/auth.rs` | read cached 2FA/admin state |
| `core-rs/crates/nc-dav/src/filesystem.rs` | `load_meta` store-on-miss; `read_dir` uses the JOIN listing |

### Verification (round 3)

1. `cargo test --lib` — extended JOIN + cache unit tests (SQLite in-memory, absent-extended-row fallback pinned).
2. `make diff-test` — 20/20 scenarios (write paths exercise the miss-path change directly — every PUT/MKCOL/DELETE).
3. Postgres statement log before/after per request class: depth-1 PROPFIND, depth-0 PROPFIND, GET, PUT-create.
4. `make bench-load` — capabilities probe should move toward status.php (auth-query reduction); record in benchmarks.md.

**Not a query-count win** (verified, deliberately out of scope): the depth-0 root's remaining ~10 singles (dav-server-rs visits the root before `read_dir`; each node is visited once per request, so memoization buys nothing beyond Task 10's meta part), and the write-path gap vs PHP (propagator already batched post-deadlock-fix; the 1.3-2.3× is DB work + fsync, not N+1 — worth a query-count measurement before touching it).

---

## Round 4 — remaining per-request queries (2026-08-10)

Follow-up to the round-3 A/B (1124 → 24 statements per 100-child depth-1
PROPFIND, latency unchanged on the local hot DB — the win materializes on
slower storage where each round trip costs ms). The remaining per-request
work, ranked for a slow-disk / low-CPU deployment:

### Task 11 — Throttle the `last_activity` UPDATE to PHP's interval (parity + write removal)

Verified against the PHP reference: `PublicKeyTokenProvider::updateTokenActivity`
(`lib/private/Authentication/Token/PublicKeyTokenProvider.php:296`) writes only
when `last_activity < now − token_auth_activity_update` (system value, **default
60 s**, clamped 0-300). Rust's `spawn_last_activity_update`
(`nc-auth/src/bearer.rs:147`) issues an unconditional `UPDATE oc_authtoken` on
**every** authenticated request — a DB write per request PHP would not do for
60 s (on HDD: a seek + journal write per request). Fix: add `last_activity` to
the token lookup + cache entry, and skip the update when within the 60 s
window. Net: ~1 write/60 s per token instead of 1/request; also a parity fix.

### Task 12 — Fold `sharing_disabled` + display name into `cached_user_state`

`sharing_disabled_for_user` runs 2-3 queries per PROPFIND root:
`oc_appconfig` `shareapi_exclude_groups` + `shareapi_exclude_groups_list`
(global config — belongs in the state-level `appconfig_cache`, 0 queries after
warmup) plus an `oc_group_user` membership query (uid-dependent — belongs in
the 60 s TTL cache next to the admin check). The `{oc:}owner-display-name`
lookup (uid-only) joins the same cache entry. Net: ~3 queries off every
PROPFIND root, depth-0 and depth-1 alike.

### Task 13 — Raise the bruteforce COUNT cache TTL (2 s → 30-60 s)

At low request rates the 2 s TTL (Phase 18.3) still pays both COUNT queries on
nearly every request. The staleness argument that justified 2 s (counts change
only on `record_attempt`; the delay formula is exponential) holds at 60 s. Net:
~2 queries per request eliminated at ≤1 req/s cadence.

### The floor (documented, out of scope)

**Why the root's `get_props` cannot use the batch.** dav-server-rs visits the
request root *before* `read_dir` (its `propfind` handler calls
`write_props(root)` then `propfind_directory`), so the root is never in the
`PropfindBatch` and every lookup falls back to the single-row queries.  Each
node is visited once per request, so per-request memoization buys nothing, and
a cross-request TTL cache would break PHP parity on writes (a share created
2 s ago must appear in the next PROPFIND).

**The ~8 per-file queries remaining on every PROPFIND root** (all in
`nc-dav/src/filesystem.rs` `get_props`, single-row fallbacks; `fc_path` =
`files` for the webdav root):

| # | query (row fn) | serves | why irreducible |
|---|---|---|---|
| 1 | `count_children` — `SELECT SUM(CASE WHEN mimetype = $dir …) … FROM oc_filecache WHERE parent = $1 AND storage = $2` | `{nc:}contained-folder-count` / `-file-count` | per-file data; changes on every write in the dir |
| 2 | `get_share_details` — `SELECT DISTINCT share_type, share_with FROM oc_share WHERE file_source = $1 …` (+ display-name batch for user-type shares) | `{oc:}share-types` / `{nc:}sharees` | per-file; a new share must appear immediately |
| 3 | `get_share_note` — `SELECT note FROM oc_share WHERE file_source = $1 AND note != '' ORDER BY stime DESC LIMIT 1` | `{nc:}note` | per-file; share edits must appear immediately |
| 4 | `get_comments_count` — `SELECT COUNT(*) FROM oc_comments WHERE object_type = 'files' AND object_id = $1` | `{oc:}comments-count` | per-file; new comments must appear immediately |
| 5 | `get_comments_unread` — `SELECT COUNT(*) FROM oc_comments c … actor_id != $2 AND creation_timestamp > COALESCE((SELECT marker_datetime …), '1970-01-01 00:00:00')` | `{oc:}comments-unread` | per-file × per-user; read markers change constantly |
| 6 | `get_system_tags_for_file` — `SELECT t.id, t.name … FROM oc_systemtag t JOIN oc_systemtag_object_mapping m … WHERE m.objectid = $1` | `{nc:}system-tags` | per-file; tag edits must appear immediately |
| 7 | `list_custom_properties` — `SELECT propertyname, propertyvalue, valuetype FROM oc_properties WHERE userid = $1 AND propertypath = $2` | custom (`oc_properties`) props | per-file × per-user; PROPPATCH writes must appear immediately |
| 8 | `get_tag_info` — `SELECT vco.objid, vc.category FROM oc_vcategory_to_object vco JOIN oc_vcategory vc … WHERE vc.uid = $1 … AND vco.objid IN ($4)` | `{oc:}favorite` / `{oc:}tags` | per-file × per-user; the root is not in `read_dir`'s tag prefetch (it runs before it) |

Plus the root's metadata (`load_meta` → `lookup_by_path_with_ext`, 1 query —
already JOIN-reduced) and, until Task 12 lands, the display-name and
sharing-config lookups.  After Tasks 11-13 the depth-1 PROPFIND floor is
~19 statements: auth (~1 cached token + 2 bruteforce counts every 30-60 s) +
read_dir batch (6) + root (8-10).

**Write path** (~15 queries per PUT): could lose ~1 via a CTE-combined
filecache-insert + extended-upsert, but it is already the fastest-vs-PHP area
(1.3-2.3×); measure the PUT query count before touching it.

With Tasks 11-13: depth-1 PROPFIND ~26 → ~19 statements, GET ~8 → ~5, and the
per-request DB **write** disappears — the highest-value item on slow storage.
