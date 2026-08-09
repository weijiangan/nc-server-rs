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
