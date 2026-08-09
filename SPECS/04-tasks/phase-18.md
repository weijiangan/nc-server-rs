# Phase 18 — Performance improvements from the Phase 17 flamegraphs

Goal: act on the cost centers the Phase 17 flamegraph pass identified — cut the per-request axum service-stack cloning by consolidating the DAV mount routes, eliminate the per-request fs stat in `try_static_files`, and cache the bruteforce COUNT queries — then re-measure to prove the deltas.

Full plan: [`SPECS/03-implementation-plan/plan/19-performance-improvements.md`](../03-implementation-plan/plan/19-performance-improvements.md).

> **Why this exists.** The first flamegraph pass (under load) showed axum's per-request clone/drop machinery (`Route::clone`, `CloneService::clone_box`, `MapFuture/MapErr/MapIntoResponse::clone`) out-sampling every handler frame — the router carries ~30 near-duplicate DAV mount routes (`router.rs:158-209`). It also showed `try_static_files` statting every GET/HEAD request and `auth_layer` running two bruteforce COUNT queries per request. This phase turns those profile findings into measured wins.

---

## Governing decisions (grounded)

- **The static whitelist is a deliberate fidelity narrowing, not a regression.** Verified against the running webroot: static files live under `/core/ /dist/ /themes/ /apps/` plus `robots.txt` and the install-time `index.html` at the root. The current code serves *any* existing file, including repo files (`AUTHORS`, `README.md`, `3rdparty/*`) and dotfiles that real nginx installs deny — blocking those is a security gain, and the two exact root files keep the parity surface that matters (nginx serves both from disk).
- **The DAV arbiter becomes the single classified entry for both mount roots.** `dav_arbiter_handler` already intercepts SEARCH/REPORT and falls through to the native handler; extending it to classify the proxied subtrees (versions/comments/trashbin/principals/calendars/public-calendars/system-calendars/addressbooks/avatars/access-control), uploads, and bulk collapses ~30 routes into 6. `dav_handler` already resolves its own strip prefix, so the fall-through is unchanged.
- **Non-POST `/dav/bulk` becomes 404 instead of 405 — more PHP-faithful.** sabreDAV treats "bulk" as an ordinary resource path for non-POST methods; the current axum 405 was an artifact of the post-only route. Documented, not hidden.
- **Bare subtree paths (`/remote.php/dav/versions` with no trailing content) now proxy to PHP** instead of falling into the native files tree (404). PHP routes them into their trees; the diff suite doesn't cover them, and the proxied result is the faithful one.
- **The registry-built per-app routes stay untouched.** Their explicit per-app 404 semantics for unknown paths is a deliberate Phase 7.5 decision (documented in `router.rs`), not clone-cost churn.
- **The throttler cache trades ≤2 s staleness for two DB queries per request.** The count only changes on `record_attempt`; the delay formula is exponential, so a stale count is immaterial. The DB stays the source of truth on miss.

---

## Verifiable stops

| Stop | Tasks | Gate |
|---|---|---|
| **S0 — Static whitelist** | 18.1 | Parity probes: `/core/img/logo.svg`, `/dist/*`, `/apps/files/img/icon.svg`, `/themes/*`, `/robots.txt`, `/index.html` → 200; `/AUTHORS` → 404; full `make diff-test` green. |
| **S1 — Route consolidation** | 18.2 | `make diff-test` green (webdav/upload/bulk/share flows traverse the arbiter); `make bench-one SC=10_put_get` improves or holds vs the Phase 17 baseline. |
| **S2 — Throttler cache + re-measure** | 18.3, 18.4 | `make bench-load` + fresh `make profile` flamegraph; clone-machinery share drops vs `profile-1786300646.svg`; benchmarks.md baseline refreshed. |

---

## Tasks

### 18.1 Static-file path whitelist

- [x] In `try_static_files` (`nc-server/src/router.rs:36-59`): before the `tokio::fs::metadata` call, require `path` to start with `/core/`, `/dist/`, `/themes/`, or `/apps/`, or be exactly `/robots.txt` / `/index.html`; otherwise fall straight through. Keep the existing `.php` and `..` checks.

### 18.2 DAV route consolidation

- [x] Replace the ~30 DAV mount routes (webdav triple, arbiter roots, 11 proxied subtrees ×2 prefixes, uploads ×2, bulk ×2, files wildcard ×2) with 6 routes: `/remote.php/dav`(+`/`) and `/dav`(+`/`) exact + `/remote.php/dav/{*path}` and `/dav/{*path}` wildcards, all to `dav_arbiter_handler`.
- [x] Extend `dav_arbiter_handler`: SEARCH/REPORT → proxy; remainder after the mount root starting with a proxied-subtree prefix → proxy; `/uploads` → `nc_dav::upload_handler`; `== /bulk` + POST → `nc_dav::bulk_handler`; else `dav_handler` fall-through.

### 18.3 Throttler COUNT cache

- [x] Cache the short/long-window counts per `(action, subnet)` in a DashMap with a 1-2 s TTL inside `check_throttle` (`nc-auth/src/bruteforce.rs`); DB remains the source of truth on miss. (429 and query-error paths deliberately not cached.)

### 18.4 Verification + baseline refresh

- [x] `cargo check` + `cargo test --lib`; static parity probes (S0); full `make diff-test` green.
- [x] `make bench-one` + `make bench-load` before/after; fresh `make profile` flamegraph; clone-machinery comparison vs `profile-1786300646.svg`; refresh the baseline tables and add the deltas to `docs/benchmarks.md`.

---

## Changes

- **2026-08-10 — Initial write.** Tasks from plan §19; deliberate behavior changes (whitelist narrowing, `/bulk` 405→404, bare-subtree proxying) recorded as governing decisions up front.
- **2026-08-10 — Phase 17 token bootstrap was producing invalid `PublicKeyToken` rows (found during S1).** The SQL-inserted `oc_authtoken` rows carried `version = 2` but NULL `private_key`/`public_key`. PHP's `updatePasswords()` — run on any subsequent plain-password login — calls `encryptPassword($password, $publicKey)` with the NULL key and 500s (`TypeError`), breaking the oracle for every diff-test scenario. Fixed: the bootstrap now generates a real RSA-2048 keypair (PKCS#8 private + SPKI public PEM, exactly PHP's `openssl_pkey_export` format), deletes stale keyless `nc-bench` rows first, and stores the PEMs in the row. 20/20 scenarios green after the fix.
- **2026-08-10 — `divergences.yaml` had committed 0x01 control characters** (`backgroundjob\x01lastjob`, `core\x01lastcron`, `circles\x01maintenance_update` — the `|` separators were corrupted), making serde_yaml reject the divergences inventory and aborting every scenario run with `control characters are not allowed`. Replaced with `|` per the phase-16 noise-key convention.
- **2026-08-10 — Measured results.** Load mode: status.php p50 2.17→1.39 ms (−36%), req/s +19%; capabilities +22% req/s; PROPFIND depth-1 +11% req/s / −12% p50. Flamegraph clone-machinery share 35.0%→33.6%. Scenario totals within noise (±7-30% on individual scenarios, shared-host). Full before/after in `docs/benchmarks.md`.
- **2026-08-10 — Write-load profiling found a concurrency bug: concurrent PUTs deadlock in Postgres.** A profile under `PUT` load (the read-only probes could never expose it) showed the SUT idle-waiting on sqlx, and the Postgres log confirmed: `deadlock detected` on the propagator's own parent-chain UPDATE (`oc_filecache … path_hash IN (…)`) — the tuple-recheck cycle. 258 deadlocks during the probe runs; PUT p50 76 ms (vs 54 ms sequential), max 2112 ms, sqlx pool churn to 29 connections, 41 req/s vs PHP's 115. The sorted `IN`-list cannot enforce a lock order — Postgres locks matches in plan-dependent scan order — so the propagator's existing defenses were cosmetic. **Fix** (`propagator.rs`): the UPDATE now runs inside an explicit transaction that first pre-locks every parent row `SELECT … WHERE storage = $1 AND path_hash IN (…) ORDER BY path_hash FOR UPDATE` — deterministic lock order, deadlock-free by construction. Postgres-only (backend_name check); SQLite's whole-file locking needs no pre-lock. After: **0 deadlocks**, max stall 141 ms, pool stable; the residual p50 (~88 ms under 4-way same-directory contention) is lock serialization on the shared directory row, inherent to the workload. The pre-lock transaction structure is exercised by the sequential differential suite: **20/20 green** (the earlier 18/20 runs were storm residue — 240+ probe PUTs had orphaned `files_versions` rows on the SUT; cleaned and re-verified).
