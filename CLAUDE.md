# Constitutions

## Core Principles

1. **Correctness above all.** Every behavior must match the canonical PHP implementation in `workspace/server/`. When in doubt, read the PHP source and replicate its logic exactly — same status codes, same headers, same edge cases. Never take shortcuts that sacrifice fidelity.

2. **Performance is the purpose.** This rewrite exists to deliver maximum performance. Every round-trip, allocation, and catalog query counts. Optimize for the hot path. When PHP does it in one query, we do it in one query. Number of lines is not a concern — correctness and speed are.

3. **Never guess at PHP behavior.** Verify against the actual PHP source code and/or the running database schema (`docker exec master-database-pgsql-1 psql ...`). The ground truth is what PHP actually does, not what a task description summarizes.

4. **Match PHP at interop boundaries, otherwise performance wins.** At critical interop points — database schema, wire protocols, HTTP responses, client-visible behavior — replicate PHP exactly. Everywhere else, Rust's native strengths (zero-copy parsing, async I/O, efficient data structures) should dominate. Don't copy PHP patterns that exist only because of PHP's limitations.

5. **Be a courageous, rigorous staff-level engineer.** You are not a ticket-closing code monkey — you are a staff engineer entrusted with rewriting a mission-critical system. Act like it.
   - **Restructure when it's right.** Don't be afraid to restructure code when the current architecture is wrong. If PHP's call-sites are inconsistent, unify them. If validation is scattered, centralize it. If a refactor makes the system more maintainable, do it — even if it touches more files than strictly necessary.
   - **Hypothesize, then verify.** When you encounter something unexpected in the PHP reference — a weaker check at one call site, a missing guard, a partial write on error — hypothesize *why* it might be that way (legacy compatibility, oversight, copy-paste), then verify against the broader context. If it's clearly a bug or inconsistency, don't replicate it. Call it out as an intentional improvement and document it. When PHP uses a framework method (`View::copy()`, `$storage->file_put_contents()`) where a language built-in would work, that's a signal — the framework carries side effects (events, cache updates, DB writes) that the built-in doesn't. Trace the full call chain, not just the visible lines.
   - **Build for maintainability.** This code will outlive this session. Keep modules focused, functions small, and responsibilities clear. Prefer well-architected over quickly-delivered. Number of lines is not a concern — correctness, clarity, and architecture are.
   - **Uphold the other principles.** This principle is a multiplier on the others. Courage means refactoring to hit PHP parity (principle 1), restructuring for performance (principle 2), and verifying rather than guessing (principle 3). Rigor means every edge case is handled, every deviation documented, and every assumption tested.
   - **Tasks can be underspecified — always verify against the source.** Task descriptions are best-effort written summaries — they can miss edge cases, misidentify PHP call sites, or elide whole categories of checks. When a task says "check `enable_previews` + mimetype," ask: is that *all* PHP checks? Trace every provider's `isAvailable`. If PHP checks three things and the task mentions two, the task is wrong — implement three. If the task says "replicate PHP" but the PHP is buggy, call it out and decide explicitly whether to replicate or fix. Never silently implement what the task says when the code says something different.

6. **PostgreSQL is the uncompromised first-class target; only Postgres and SQLite exist.** The only supported databases are PostgreSQL (production, best-in-class, never compromised) and SQLite (test/legacy). There is no MySQL/MariaDB — don't write for it. When a code path must branch on dialect, write the Postgres path first using native types and idiomatic SQL, and accommodate SQLite only where its limitations force it. Never let SQLite's constraints degrade the Postgres hot path: no `CASE` projections, type coercions, or lowest-common-denominator workarounds on Postgres to satisfy SQLite. Isolate any SQLite accommodation behind a dialect check (e.g. cache the backend once) so the Postgres path stays clean, native, and fast. When a dialect difference affects a client-visible value (an `ETag`, a header, a stored format), match what PHP produces on **Postgres** exactly — verify against the live database, not the SQLite test fixture.

## Engineering Hygiene

1. **Log errors, don't swallow them.** `let _ =` discards errors silently. When a database query, filesystem operation, or network call can fail, log the error at `warn!` or `error!` level — especially for side-effecting operations like cache invalidation where the failure is invisible to the caller. A silent failure here cost hours of debugging a "working" fix that was hitting a nonexistent table.

2. **When the PHP path works and the Rust path doesn't, test the asymmetry directly.** The user's "restore → restore works" was a critical diagnostic signal — it proved PHP-FPM's own pipeline was healthy and the gap was specifically what Rust *wasn't* doing. Chase that asymmetry early rather than patching around symptoms. Ask: what side-effects does PHP trigger (events, cache invalidation, metadata updates) that Rust skips?

3. **PHP framework abstractions carry hidden side effects — trace the full call chain.** When PHP calls a framework method that has a language-built-in equivalent (`View::copy()` vs `copy()`, `$storage->file_put_contents()` vs `file_put_contents()`), the framework is doing more than the built-in. It runs through storage wrappers, the cache layer, the scanner, and dispatches events — each of which can trigger listeners that write to other tables. A few lines of PHP can silently insert rows, update caches, and populate JSON metadata columns in tables you didn't know existed. Events are part of the call chain — one event dispatches another, each with its own listeners. When replacing such a call with a raw Rust equivalent, walk every side effect downstream of the original, including every event dispatched and every listener registered for it. The abstraction exists because the built-in isn't enough. [[phps-framework-abstractions-carry-hidden-side-effects]]

4. **Adversarially verify equivalence claims.** When a deviation claims Rust's approach produces an "identical" outcome, don't verify by asking whether a row exists or a file was created. Ask: *what downstream consumers read this data, and what exact fields do they require?* Then check every one of those fields against what PHP writes. If a downstream PHP-FPM endpoint reads column X to serve property Y, and Rust didn't populate X, the outcome is not identical — no matter how correct the visible operation appears. [[adversarially-verify-equivalence-claims]]

5. **Recognize framework-abstraction signals.** When PHP uses a framework method where a language built-in would work, that's not an accident — it's a signal that the framework carries side effects the built-in doesn't. `View::copy()` over `copy()`, `$storage->file_put_contents()` over `file_put_contents()`, a DI-injected service over a static call. Each of these is a cue to apply rule 3: trace the full call chain before replacing it with a raw Rust equivalent. [[recognize-framework-abstraction-signals]]

6. **PHP events are a cascade, not a single dispatch.** When PHP dispatches event A, handlers for A almost always dispatch events B and C. Tracing `CreateVersionEvent` → zero listeners is correct but misleading — `View::copy()` also dispatches `NodeCreatedEvent` and `NodeWrittenEvent`, each with their own listeners that write to tables. When walking a PHP call chain, don't stop at the first event you find; search for every `dispatchTyped` and `->dispatch(` reachable from the call, then check every registered listener for each event. [[php-events-are-a-cascade]]

7. **Beware of read operations that write.** Methods named `getVersionsForFile`, `listX`, `findY` sound like pure reads — but they often INSERT missing rows or DELETE orphaned ones as a "sync" side effect. PHP's `getVersionsForFile` inserts `oc_files_versions` rows with `metadata=[]` for any filesystem version that lacks a DB row. These hidden writes collide with Rust's inserts and create the exact kind of silent data mismatch that takes hours to debug. When PHP "syncs" during a read, Rust must either replicate the sync or ensure the data is already consistent. [[beware-read-operations-that-write]]

## Documentation Conventions

**Applies to every document:**

- **The gate is the definition of done — never a result.** A change is done only when its gate passes; reporting "green", "unchanged counts", or "no regression" states the bar, not the outcome. Report what changed: numbers, deltas, findings.
- **One commit per doc change — amend, don't pile.** When a doc edit needs revising, fold it into the original commit instead of stacking a new one.
- **Each document owns one thing.** Benchmarks = measurements; task docs = execution history; specs = PHP behavior. Don't copy another document's content into yours.

**Phase task docs (`SPECS/04-tasks/phase-*.md`):**

1. **Never modify original task descriptions.** The PHP/Rust-gap/Verify text is written as a best-effort spec up front and stays verbatim — even when it later proves wrong (a stale capture, a misidentified call site). Correct it with a note *below* the task, never by editing the task body.
2. **Mark done / not done via the checkboxes only.** That is the status signal; don't rewrite a task to reflect its outcome.
3. **A deviation = a departure from the original task description** (what it claimed or expected), *not* a divergence from PHP. If a fix simply makes Rust match PHP, there is no deviation — don't write one.
4. **No redundant notes.** A note that just repeats what the task already asks for is dead weight — remove it. Keep a deviation only where it genuinely departs from the task text.
5. **History lives in the per-phase `## Changes` log at the bottom**, not in the task body: what was tried, what was reverted and why, root causes, superseded analyses.
6. **Ground claims before writing them.** Verify against the PHP source and/or the live A/B harness; do not carry a handover doc's assertions forward unverified — and never present an unconfirmed finding as a confirmed requirement.

**Specifications (`SPECS/01-requirements/`):**

1. **They specify the PHP server's behavior — the target.** Describe exactly what PHP does (headers, XML shape, status codes, DB writes, edge cases), grounded in PHP source with `file:line` (or `file:function`) citations.
2. **No implementation information.** No Rust state, commit hashes, vendored-crate internals, "what we tried / ruled out / matched for parity", testing narratives, or harness details. (Stating *which sub-tree Rust serves vs. delegates* is fine — that is architecture, not implementation state.)
3. **Spec = PHP behavior even where we intentionally diverge.** If Rust deliberately differs, that decision belongs in `SPECS/02-specifications/improvements.md` and the phase docs — the requirement still records what PHP does.

**Benchmarks (`docs/benchmarks.md`):**

1. **Performance data and its significance only.** Measurements (latency tables, statement-text counts) plus *why the numbers behave as they do*. Implementation — flags, files, fix mechanics, the fix story — goes in the phase task docs.
2. **Never report the gate as a result** (green, unchanged counts, no regression) and don't duplicate owned data (`perf-budget.yaml`, phase `## Changes` logs).

## Project Context

- **Workspace**: `~/Git/nextcloud-rewrite/nc-server-core/workspace/server/` (a submodule pinned at `e2dc439c7157e6864313d19e90e626a5db7f20bf`) contains the PHP reference implementation. Verify against the current state when tracing PHP behavior.
- **Database**: `docker exec master-database-pgsql-1 psql -U postgres -d nextcloud` for live schema verification
- **Migrations**: `core-rs/migrations/` are exercised against SQLite in tests; production PostgreSQL schemas are created by PHP Doctrine migrations
- **Packaging**: `core-rs/packaging/` contains an Arch Linux PKGBUILD that copies the real source tree at build time (`prepare()` copies from the parent `core-rs/` directory).  The `packaging/src/` subtree is a stale build artifact — **do not** update it after code changes; it will be refreshed on the next package build.
- **Test scope**: `cargo test --lib` for unit tests; integration tests (`cargo test`) may fail on pre-existing issues

## Dev Docker — A/B Testing & Rebuilds

The only verification target is the local dev docker (`master-*` containers, podman with the docker CLI shim). `master-nextcloud-1` runs **both** the Rust `nc-server` (`:80`) and php-fpm; `master-proxy-1` exposes a clean A/B on the same database/instance — the two share `master-database-pgsql-1`, so responses are directly comparable:

| entry | URL | path |
|---|---|---|
| Rust | `http://<lan-ip>:8080` | proxy `default_server` → `nextcloud:80` |
| PHP  | `http://<lan-ip>:9090` | nginx vhost → php-fpm TCP `nextcloud:9000` (bypasses Rust) |

**Comparing an endpoint** (same creds, same DB):
```bash
curl -s -u admin:admin "http://127.0.0.1:8080/<path>"   # Rust
curl -s -u admin:admin "http://127.0.0.1:9090/<path>"   # PHP
```
For `oc:`/`nc:` DAV properties, send an **explicit** `<d:propfind><d:prop>…` body — an allprop/bare PROPFIND does not emit them.

**Rebuilding** (run from the repo root; Docker is podman):
```bash
make sut-image    # rebuilds master-nextcloud:latest — Rust source enters via podman --build-context rustsrc=core-rs (compose can't pass it)
make up           # compose up (proxy rebuilt for the baked vhosts) + `restart proxy` (REQUIRED after recreating nextcloud: the proxy caches the old upstream IP and 502s otherwise)
make diff-up      # sut-image + up + wait for both instances installed:true (SUT :8080, oracle :9091)
make diff-test    # differential suite: cargo test -p nc-difftest --release -- --ignored
make diff-one S=27 # a single scenario
```
The php84 image is shared by the `nextcloud` and `oracle` compose services (`image: master-nextcloud:latest`) — the oracle is the SUT's byte-identical twin by construction.
- Logs: `docker logs master-nextcloud-1` · DB: `docker exec master-database-pgsql-1 psql -U postgres -d nextcloud`
- The php-shim is baked into the image at `/usr/local/share/nc-server/php-shim/index.php`. For a quick live test, `docker cp core-rs/php-shim/index.php master-nextcloud-1:/usr/local/share/nc-server/php-shim/index.php` (PHP reads it per-request, no restart) — rebuild to persist.

### Differential-test quiescence — `oc_preview_generation`

**Slow scenarios = stuck preview queue, not flakiness.** Every file write queues a row into `oc_preview_generation`; the drainer (`OCA\PreviewGenerator\BackgroundJob\PreviewJob`) registers in `oc_jobs` only at app install. First check: `docker exec master-database-pgsql-1 psql -U postgres -d <db> -c "SELECT count(*) FROM oc_preview_generation"` — SUT = `nextcloud`, oracle = `oracle` (separate databases!). Rows sit → drainer missing → re-run the install flow (`occ app:disable previewgenerator && occ app:enable previewgenerator`; the fork's `scripts/enable-preview-imaginary.sh` does this on `down -v` recovery). The runner matches the drainer by its **exact** class — don't weaken the match (`%PreviewJob` also matches `OC\Preview\BackgroundCleanupJob` and drains nothing).

Stall rules (2026-08-10):
- **A one-sided divergence is a bug until proven timing.** Find why the sides differ before masking it; the harness must catch regressions, not paper over them.
- **Don't silence the harness with config switches or `noise` entries** (`enable_previews => false` disables generation entirely; a `noise` mask once hid a broken SUT for a whole session).
- **The dev `config.php` is drift-prone operational state** — when reconstructing it, re-verify `trusted_domains` (the difftest sends `Host: nextcloud.local`; if missing, the OCS quota scenario 400s while DAV paths skip the check).

### Differential-test operations — lessons from the 2026-08-13 milestone

- **`make diff-test` runs only `preconditions_pass`** — the real suite is `difftest run <scenario.yaml>` per YAML (loop over `crates/nc-difftest/scenarios/*.yaml`); run it at milestones, not per stop.
- **The replay must consume PROPFIND response bodies.** dav-server-rs streams `read_dir` + the batch work inside the body stream; a dropped body means the SUT's read-path work never executes during the replay while PHP builds eagerly — the DB-delta comparison then measures nothing on the SUT side (the root cause of a multi-hour divergence hunt; the fix in `scenario.rs` is load-bearing, keep it).
- **`down -v` before a milestone run** (wipes both DB volumes → symmetric fresh install). Then wait for **full readiness**, not just `installed:true`: installing.html gone, all seeded users' data dirs present, ~90 s settle — background `occ user:add` and install tail-writes land after installed:true and race early scenarios.
- **Fresh-install first-access artifacts**: PHP lazily materializes the user's `cache/` row + bumps the storage root (etag + storage_mtime) on the first home access (`View.php:1396-1417` shallow scan when the row is incomplete). Rust replicates it on the read path (once per storage per process) with an **un-collidable** storage_mtime bump (`GREATEST(+60)`) — the harness masks timestamp values and compares changed-column *sets*, so a same-second write reads as "unchanged".
- **Rebuilds**: `make sut-image`'s `ADD https://github.com/…` step fails intermittently (network EOF). Never pipe the build output (`| tail` masks the failure; compose then recreates from the stale image) — run unmasked with retries, and verify the deployed binary by grepping a **string literal** from the change (identifiers don't survive compilation).
- **Diagnostics**: count `execute sqlx` lines in the PG statement log (`log_statement='all'`, ms-precision timestamps — docker logs are UTC, the host is +08:00). `pg_prepared_statements` is per-session — useless cross-session. The proxy access log's response sizes distinguish a full depth-1 listing (~11 KB) from a root-only response (~2.8 KB).
- **One-sided `oc_appconfig` timestamps** (`lastcron`, `lastjob`, `lastupdatedat`) are cron-timing artifacts (updatenotification's VersionCheck rewrites `lastupdatedat` when >30 min stale) — not code bugs. Nothing is added to `divergences.yaml` without explicit operator permission.
