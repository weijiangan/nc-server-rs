# Phase 16 — Differential Integration-Test Harness (Rust core vs. PHP reference)

Goal: an out-of-process **differential oracle** that runs the *same* HTTP operation sequence against the Rust `nc-server` (SUT) and a pure-PHP instance (Oracle), then diffs the resulting **PostgreSQL state** and **on-disk file tree**. Any divergence is a bug signal. It is the equivalence gate before deploy and in CI.

> **Why this exists.** The rewrite's premise is behavioral identity with PHP (CLAUDE.md principles 1, 3, 4), and its recurring failure mode is **silent DB side-effect divergence**: Rust performs the visible operation correctly but skips a downstream write PHP would have made — a cache row, an etag bump, an `oc_filecache_extended` entry, a metadata JSON column (CLAUDE.md hygiene rules 3, 4, 7). Protocol suites (Behat/litmus) cannot catch these because the HTTP response still looks right. Comparing **database + filesystem deltas** can.

The harness is a new test-only crate **`nc-difftest`** in the `core-rs` workspace. It is black-box: it speaks HTTP to the two live instances and SQL to their two databases. It does **not** link `nc-server` or any `nc-*` crate.

---

## Governing decisions (grounded)

- **The Oracle is a clone of the `nextcloud` service, not `nextcloud2`.** The SUT (`nextcloud`) is built from the *local* `php84/Dockerfile` (PHP 8.4, custom `bootstrap.sh`, Rust `nc-server` on `:80`, php-shim, `WITH_REDIS=YES`, the php-fpm TCP pool). `nextcloud2` is the **stock** `nextcloud-dev-php${PHP_VERSION}` image — the same PHP major (`.env` sets `PHP_VERSION=84`) but a *different image* with its own entrypoint/bootstrap/nginx and none of the SUT's local env wiring (no `WITH_REDIS` → a different caching backend; no `NEXTCLOUD_AUTOINSTALL_APPS` → a different app set; no php-shim, no php-fpm-tcp pool) — using it would flood the differential with that config drift. So the Oracle is a new `oracle` compose service cloned from `nextcloud` (same image/env/`additional.config.php`/skeleton) on **its own DB and file tree**, reached as pure PHP through a proxy vhost (16.1). This resolves the plan's "two-instance config drift" risk by construction.
- **Compare deltas, not absolute state.** SUT and Oracle use independent Postgres sequences/snowflakes, so the same logical row has different ids. Snapshot **before** and **after** on each side and diff the *delta* — this cancels install-time differences (instanceid, auto-id watermarks, baseline rows).
- **The fileid-sequence offset is solved by a natural-key id-bijection** (16.5): rows are matched across sides by a stable natural key (`oc_filecache` by `(storage, path)`, `oc_mimetypes` by `mimetype`, …) in FK-topological order, and every id/FK remaps through a canonical label. A row present on one side but not the other under a natural key is itself a reported divergence — never masked.
- **Over-masking hides bugs; under-masking floods false positives.** Column classification is explicit in `column_registry.yaml` (16.4/16.5) and unit-tested with hand-built fixtures. Timestamps are masked to preserve **equality/ordering** relationships within a row (so a missed mtime bump is still caught), not blanket-zeroed.
- **The harness must be proven to catch bugs** (negative control, 16.6) and **proven to not false-positive on proxied paths** (self-check, 16.7). A harness that always passes is worthless; one that always fails is noise.
- **PostgreSQL is the target** (CLAUDE.md principle 6). The harness reads the live Postgres at `127.0.0.1:8212`. Column types/forms are verified against the live schema, not the SQLite fixture.

---

## Verifiable stops

Each stop ends in a concrete, demonstrable gate. Do not proceed past a stop whose gate is red.

| Stop | Tasks | Gate |
|---|---|---|
| **S0 — Oracle infra** | 16.1 | SUT (`:8080`, Rust) and Oracle (`:9091`, pure PHP) both answer `status.php`; DBs `nextcloud` + `oracle` exist with the same `oc_version` and enabled-app set. |
| **S1 — Pipeline skeleton** | 16.2, 16.3 | The crate snapshots both DBs in one consistent txn and replays one op against both base URLs. |
| **S2 — First green slice** | 16.4, 16.5 | `10_put_get_delete` runs end-to-end and reports **identical**; canonicalizer unit fixtures pass. |
| **S3 — Harness self-validation** | 16.6, 16.7 | A seeded divergence in `nc-dav` is **caught** (then reverted); the proxied self-check **always passes**. |
| **S4 — File tree + core ops** | 16.8, 16.9 | File-tree deltas match; scenarios 11–18 report identical. |
| **S5 — Breadth** | 16.10, 16.11 | Scenarios 20–24 and preview-row-shape report identical. |
| **S6 — Wiring** | 16.12, (16.13) | `make diff-test` runs the whole suite; `cargo test --lib` is unaffected; CI gate green. |

---

## S0 — Oracle infrastructure

### 16.1 Stand up the pure-PHP `oracle` instance (clone of `nextcloud`)
> Grounded in the live repo: the SUT image is `docker/php84/Dockerfile` (Rust `nc-server` bound to `:80` via `setcap`, launched unconditionally by `docker/bin/bootstrap.sh:451`; the image has **no nginx**). Pure PHP is reachable today only via the proxy vhost `docker/nginx/my_proxy.conf` (`listen 9090` → `nextcloud:9000`, the php-fpm **TCP** pool added by `docker/configs/php-fpm-tcp.conf`). The bootstrap derives the DB name from `VIRTUAL_HOST` (`docker/bin/bootstrap.sh:239`). `.env` sets `COMPOSE_PROJECT_NAME=master`, `SQL=pgsql`, `PHP_VERSION=84`, `PORTBASE=821`.

Build the Oracle by cloning the SUT's service definition and bypassing Rust:

- [ ] Add an `oracle` service to `docker-compose.yml`, a near-verbatim copy of `nextcloud`: same `build: ./docker` / `php84/Dockerfile`, same `environment` (`SQL`, `NEXTCLOUD_AUTOINSTALL`, `NEXTCLOUD_AUTOINSTALL_APPS`, `WITH_REDIS: "YES"`, `PRIMARY`, `PHP_XDEBUG_MODE`), same mounts (`${REPO_PATH_SERVER}:/var/www/html`, `additional.config.php:ro`, `./data/skeleton/:/skeleton`, `./data/shared:/shared`) and the **same** `php-fpm-tcp.conf → zzzz-tcp.conf:ro` mount, same `depends_on` (`database-pgsql`, `redis`, `mail`).
  - Divergences from `nextcloud`, and only these: its **own** named volumes (`data-oracle`, `config-oracle`, `apps-writable-oracle`) so it gets a separate file tree; `VIRTUAL_HOST: "oracle${DOMAIN_SUFFIX}"` so the bootstrap installs into a **separate DB `oracle`**; drop `RUST_LOG` (not needed, harmless either way).
  - `nc-server` still boots inside `oracle` (bootstrap starts it unconditionally) and connects to DB `oracle` — it is simply never routed to. Do not add a toggle; the bypass is at the proxy.
- [ ] Add a proxy vhost to `docker/nginx/my_proxy.conf` mirroring the existing `:9090` block but targeting the oracle: `upstream oracle-php { server oracle:9000; }` and a `server { listen 9091; … location ~ \.php { fastcgi_pass oracle-php; … } }` using the same official front-controller `fastcgi_params` as the `:9090` block. Expose `9091` in the `proxy` service `ports`. Static-asset proxying (cosmetic for browser rendering) may point at `oracle:80` or be omitted — the harness issues API/DAV requests only.
- [ ] Add a `diff-up` target to the **repo-root** `Makefile`: `docker compose up -d nextcloud oracle database-pgsql redis proxy`, then wait until both instances report `installed: true` (poll `status.php`).
- [ ] Rebuild/recreate cleanly per CLAUDE.md: `docker compose up -d --build nextcloud oracle` then `docker compose restart proxy` (the proxy caches upstream IPs and 502s otherwise).

**Verify (gate S0):**
- `make diff-up` succeeds; `curl -s http://127.0.0.1:8080/status.php` (SUT) and `curl -s -H 'Host: oracle.local' http://127.0.0.1:9091/status.php` (Oracle) both return `"installed":true`.
- `docker exec master-database-pgsql-1 psql -U postgres -lqt` lists **both** `nextcloud` and `oracle`.
- **Parity precondition (manual, but mandatory):** `select value from oc_appconfig where appid='core' and configkey='lastupdatedat'` aside, the two DBs report the **same** `oc_version` (`SELECT value FROM oc_appconfig WHERE appid='core' AND configkey='version'`) and the **same enabled-app set** (`SELECT appid FROM oc_appconfig WHERE configkey='enabled' AND value='yes' ORDER BY 1`). Record any residual difference; if the app sets differ, the differential is meaningless until they match. Confirm both ran `occ background:cron` (`SELECT configvalue FROM oc_jobs …` or `occ background:cron` already set at install — `bootstrap.sh:331`).

> **Note:** the parity precondition above is automated in 16.2 as `preconditions.rs`; here it is a manual sanity gate so the harness is never built on drifted instances.

---

## S1 — Pipeline skeleton

### 16.2 `nc-difftest` crate scaffold, config, HTTP client, preconditions
> Reuses workspace deps (`core-rs/Cargo.toml`): `reqwest` 0.12 (rustls), `sqlx` 0.8, `tokio` 1, `serde`/`serde_json`, `clap` 4, `anyhow`/`thiserror`, `tracing`, `sha2`, `hex`. **New** deps to add to `[workspace.dependencies]`: `serde_yaml`, `similar` (unified diffs), `pretty_assertions` (dev).

- [ ] Create `core-rs/crates/nc-difftest` and add `crates/nc-difftest` to the workspace `members`. Layout per the plan: `src/{lib,config,client,db,fs,canonicalize,delta,scenario,preconditions,report}.rs`, `src/bin/difftest.rs`, `tests/differential.rs`, plus `column_registry.yaml`, `scenarios/*.yaml`, `fixtures/`. The crate depends on **no** `nc-*` crate (black-box).
- [ ] `config.rs`: base URLs (SUT `http://127.0.0.1:8080`, Oracle `http://127.0.0.1:9091` + `Host: oracle.local`), DSNs (`postgres://postgres:postgres@127.0.0.1:8212/nextcloud` / `…/oracle`), container names (`master-nextcloud-1` / `master-oracle-1`), admin creds. **All env-overridable** (`NC_DIFFTEST_SUT_URL`, `…_ORACLE_URL`, `…_SUT_DSN`, `…_ORACLE_DSN`, …) with these defaults.
- [ ] `client.rs`: `NextcloudClient` over `reqwest` with basic auth and a **Nextcloud desktop-client User-Agent** (avoids PHP's CSRF token requirement for browser UAs). Methods: `put`, `get`, `delete`, `mkcol`, `move`, `copy`, `propfind` (Depth header), `proppatch`, `lock`, chunked-upload v2 (MKCOL upload dir → PUT chunks → MOVE assembly), `bulk` (`/dav/bulk`), and OCS `share_create`. Each returns a raw response captured for normalization (16.4).
- [ ] `preconditions.rs`: assert both instances up, same `oc_version`, same enabled-app set (the 16.1 manual gate, automated). Fail fast with a precise message naming the first mismatch.
- [ ] `tests/differential.rs` is gated behind `#[ignore]` (run via `-- --ignored` / `NC_DIFFTEST=1`) so plain `cargo test --lib` (the project's unit-test entrypoint, CLAUDE.md) is unaffected.

**Verify:** `cargo build -p nc-difftest` succeeds; a smoke binary/test runs `PROPFIND Depth:0` on the home root against **both** base URLs and gets `207` with the same resource member set; `preconditions` passes on the S0 instances.

### 16.3 `db.rs` — consistent PostgreSQL snapshot
> Ground truth for column types/forms is the **live** schema (`docker exec master-database-pgsql-1 psql …`), per CLAUDE.md principles 3 and 6 — not the SQLite fixture.

- [ ] Enumerate `oc_%` tables from `information_schema` (or `pg_tables`); **assert the two DBs expose the same table set**, modulo an explicit skip-list. **Warn (do not silently drop) on any unknown table** — an unclassified table is a coverage gap, not noise.
- [ ] Dump each table with `SELECT * ORDER BY <pk>` inside a **single `REPEATABLE READ` transaction** (one consistent cross-table view per side; never commit). Rows are captured as an ordered, typed map per table.
- [ ] Skip-list (per plan §3): `oc_sessions`, `oc_jobs`, any `*_queue`. Column-level skip: `oc_authtoken.last_activity` / `last_check` (updated per request). Everything else is snapshot and classified (16.4/16.5).

**Verify:** snapshot both DBs; table sets match; core tables (`oc_filecache`, `oc_storages`, `oc_mimetypes`) dump non-empty; two back-to-back snapshots of an **idle** instance are identical (no residual background writes — if not, the quiescing in 16.5/16.8 is incomplete).

---

## S2 — Canonicalization, delta, report → first green slice

### 16.4 Scenario runner + minimal canonicalize/delta/report + initial registry
- [ ] `scenario.rs`: YAML loader for scenarios (ordered list of typed ops — `put`, `get`, `mkcol`, `move`, `copy`, `delete`, `propfind`, `proppatch`, `chunked_upload_v2`, `bulk`, `share_create` — with method/path/headers/body refs to `fixtures/`). The runner replays the **identical** sequence against both base URLs.
- [ ] **Response normalization:** compare status + selected headers + body, **minus** volatile headers (`ETag`, `Date`). This is a secondary signal; the primary oracle is the DB/file delta.
- [ ] **Double-run flake detection** (plan §5): run each scenario twice; a divergence that appears *only* on the second run is flagged as residual flakiness to investigate — never masked.
- [ ] `delta.rs`: `Snapshot → Delta` (added / changed / removed rows per table) between the before and after snapshots of one side.
- [ ] `canonicalize.rs` (minimal first cut): column classification driver keyed on `column_registry.yaml`, with the **masking** classes (`stable`, `ignore`, `timestamp_wall`, `volatile_value`, `volatile_independent`) but id-remapping stubbed to identity. Full id-bijection lands in 16.5.
- [ ] `report.rs`: render `delta_sut` vs `delta_oracle` (after canonicalization) as a unified diff via `similar`; non-empty diff = failure with an actionable, per-table/per-row report.
- [ ] `column_registry.yaml` (initial): classify the **diff-set** tables' columns for the core file ops — `oc_filecache`, `oc_filecache_extended`, `oc_files_metadata`, `oc_storages`, `oc_mimetypes`, `oc_properties`. **Verify every classification against the live schema** before committing it.
- [ ] Write `scenarios/10_put_get_delete.yaml` + `fixtures/hello.txt`.

**Verify (gate S2a):** `10_put_get_delete` runs end-to-end against the S0 instances and the report is **identical** (empty diff) on a known-good build. This validates the whole chain: client → snapshot → delta → canonicalize → report.

### 16.5 Full canonicalization — natural-key id-bijection + equality-preserving masking
> The design centerpiece (plan §3). Unit-test everything here with hand-built fixtures; prefer preserving relationships over blanket masking.

- [ ] **Column classification taxonomy** (`column_registry.yaml`, keyed `table.column`):
  - `stable` — compare verbatim (path, name, size, permissions, checksum, mimetype *name*, storage `id` string, property name/value, share perms).
  - `id_pk` / `id_fk` — remap through the canonical bijection.
  - `timestamp_wall` — mask the absolute value but **preserve equality/ordering across columns in the same row** (so `creation_time == upload_time` on a fresh PUT must hold on both sides; a missed bump is still caught).
  - `volatile_value` — random/time-based but equality is meaningful (etag): mask to per-snapshot sentinels that keep equal-values-equal and distinct-values-distinct (catches "parent got the same etag as its child").
  - `volatile_independent` — per-row random, no equality expected (share `token`, `metadata_etag`): mask to a constant.
  - `ignore` — known irrelevant (`oc_storages.last_checked`).
- [ ] **id-bijection:** build bidirectional `sut→canonical` / `oracle→canonical` maps in **FK-dependency (topological) order**, matching rows by a stable natural key — not by id:
  - `oc_storages` key `id` (`home::admin`); `oc_mimetypes` key `mimetype`; `oc_vcategory` key `(uid, type, category)`.
  - `oc_filecache` key `(canonical(storage), path)` — **path is the true natural key**. Then `oc_filecache_extended` by `canonical(fileid)`; `oc_vcategory_to_object` by `(canonical(objectid), canonical(categoryid), type)`; `oc_properties` by `(userid, propertypath, propertyname)`; `oc_files_trash` by `(user, id, location)`; `oc_share` by `(uid_owner, uid_initiator, item_type, canonical(item_source), share_with, file_target)`; `oc_preferences` / `oc_appconfig` by their natural keys (no id column).
  - Every matched pair gets a canonical label; every `id_fk` remaps through the same map. **A row present on one side but not the other under a natural key is itself a reported divergence (never masked).** Works identically for snowflake ids (`oc_previews`) since only uniqueness + natural-key matching matter.
- [ ] Complete `column_registry.yaml` for the full **diff set**: `oc_filecache`, `oc_filecache_extended`, `oc_files_metadata`, `oc_storages`, `oc_mimetypes`, `oc_properties`, `oc_vcategory`, `oc_vcategory_to_object`, `oc_files_trash`, `oc_previews` (+`_versions`/`_locations`), `oc_share`, `oc_preferences`, `oc_appconfig`. **Self-check set** (proxied, must be identical): `oc_users`, `oc_accounts`, `oc_groups`, `oc_group_user`.
- [ ] **Quiescing masks** (plan §5): mask per-request columns (`oc_authtoken.last_activity`); the skip-list from 16.3.

**Unit tests:** `cargo test --lib -p nc-difftest` (un-gated) —
- `id_offset_hidden`: two snapshots identical modulo a constant id offset + FK ripple → empty diff.
- `natural_key_mismatch_reported`: a row present on one side only under a natural key → reported divergence, not masked.
- `timestamp_equality_preserved`: `creation_time == upload_time` on both sides → equal; a missed bump on one side → diff.
- `volatile_equality_preserved`: equal etags stay equal, distinct etags stay distinct, after masking.
- `registry_coverage`: every diff-set table.column is classified (no unclassified column slips through).

**Verify (gate S2b):** `10_put_get_delete` still reports identical with the full bijection active; all canonicalizer unit fixtures pass.

---

## S3 — Harness self-validation

### 16.6 Negative control — prove the harness catches a real divergence
> Plan §Verification. A harness that passes silently is worse than none.

- [ ] Temporarily introduce a **deliberate** divergence in `nc-dav` on a branch — e.g. skip the `oc_filecache_extended` insert on PUT, or don't bump the parent etag on a child write (exactly the silent-side-effect class this harness targets).
- [ ] Rebuild the SUT image (`docker compose up -d --build nextcloud`), re-run the affected scenario.
- [ ] Confirm the scenario **fails** with a **precise** delta naming the missing/changed row and column — not a generic error.
- [ ] Revert the divergence; confirm the scenario is green again.

**Verify (gate S3a):** the seeded bug is caught with an actionable delta; after revert, identical. Record the episode in the `## Changes` log (what was seeded, what the delta showed).

### 16.7 Proxied self-check scenario (`30_share_create_selfcheck`)
> Share creation is **proxied to PHP** on the SUT (not a native Rust path), so SUT and Oracle execute the *same* PHP code on their respective DBs. This scenario must therefore **always** match — it validates the harness, not the server.

- [ ] Write `scenarios/30_share_create_selfcheck.yaml` (OCS share create over a fixture file; exercise the `oc_share` + `oc_vcategory`/property writes).
- [ ] Wire it as a standing health check: if it ever fails, the **harness** is wrong (over-masking, snapshot inconsistency, oracle drift) — investigate the harness before suspecting the server.

**Verify (gate S3b):** `30_share_create_selfcheck` reports identical on the first and every subsequent run; deliberately breaking a canonicalizer rule (e.g. mis-masking `oc_share.token`) makes it fail, confirming sensitivity.

---

## S4 — File tree + core native scenarios

### 16.8 `fs.rs` — file-tree snapshot + delta
> Plan §5. Compare bytes, not just DB rows — a correct DB row with a wrong/missing file on disk is still a divergence.

- [ ] Snapshot `data/{user}/files/**` by **relative path + size + sha256** via `docker exec master-nextcloud-1` / `master-oracle-1` running `find … -print0 | xargs -0 sha256sum`. **Exclude** volatile subtrees: `files_versions/`, `cache/`, `appdata_*/`, and in-flight `*.part`.
- [ ] Compute the file-tree delta (added/changed/removed by relative path) before→after on each side, and diff `delta_sut` vs `delta_oracle`.

**Verify:** a PUT of a fresh file yields matching file-tree deltas on both sides (same relative path, size, sha256); deleting it removes it on both; an idle double-snapshot is empty (quiesced).

### 16.9 Core native scenarios (11–18)
> Native = Rust writes the DB itself (`/remote.php/webdav/*`, `/remote.php/dav/files/{uid}/*`) — the highest-value differential surface.

- [ ] Author and green: `11_mkdir_nested`, `12_move_rename`, `13_copy`, `14_propfind_depth1`, `15_proppatch_favorite_tags`, `16_overwrite_put` (copy-on-write path), `17_delete_to_trash`, `18_explicit_mtime`.
  - `18_explicit_mtime` sends `X-OC-Mtime` so mtime-preservation is checked **deterministically** despite timestamp masking (the value is `stable`-equivalent because the client dictates it).
  - `15` exercises `oc_properties` + `oc_vcategory`/`oc_vcategory_to_object` (favorites/tags).
  - `17` exercises `oc_files_trash` (natural key `(user, id, location)`).

**Verify (gate S4):** every scenario in 11–18 reports identical (DB delta **and** file-tree delta); `make diff-one S=<name>` green for each.

---

## S5 — Breadth

### 16.10 Upload / edge scenarios (20–24)
- [ ] Author and green: `20_chunked_upload_v2` (MKCOL upload dir → PUT chunks → MOVE assembly over `/remote.php/dav/uploads/*`), `21_bulk_upload` (`/dav/bulk`), `22_invalid_filename` (**rejection parity** — same status code + error shape on both sides), `23_quota_exceeded` (same rejection), `24_checksum_upload` (`OC-Checksum` header → `oc_filecache.checksum` parity).

**Verify (gate S5a):** 20–24 report identical; the rejection scenarios (22, 23) match on status **and** normalized error body, not just "both failed."

### 16.11 Preview scenarios (DB-row shape)
> Previews need Imaginary. Unless the **same** Imaginary is configured for both instances, scope these to DB-row **shape** (`oc_previews` / `oc_preview_versions` / `oc_preview_locations` columns, matched by the snowflake-id-tolerant natural key), skipping the generated **bytes** (which can differ across libvips runs).

- [ ] Author a preview scenario that uploads a previewable image and compares the resulting `oc_previews` row shape (width/height/cropped/max/mimetype ids/`version_id`/`etag`=source-etag semantics) on both sides; exclude preview bytes from the file-tree diff's `appdata_*/` exclusion already covers them.
- [ ] Document the Imaginary precondition: either configure one Imaginary for both, or assert row-shape-only.

**Verify (gate S5b):** preview row shape matches on both sides for a known-previewable upload; a non-previewable type produces no `oc_previews` row on either side.

---

## S6 — Wiring + stretch

### 16.12 `make diff-test` / `diff-one`, CI gate
- [ ] Add to the **repo-root** `Makefile`:
  ```make
  diff-test:  cd docker/nc-server-core/core-rs && cargo test -p nc-difftest --release -- --ignored
  diff-one:   cd docker/nc-server-core/core-rs && cargo test -p nc-difftest --release -- --ignored $(S)
  ```
  (`diff-up` from 16.1.)
- [ ] Confirm the `#[ignore]` gating: plain `cargo test --lib` across the workspace is **unaffected** (no network, no DB).
- [ ] Add a CI gate that runs `make diff-up && make diff-test` against a known-good build and fails the pipeline on any non-empty diff.

**Verify (gate S6):** `make diff-test` runs all scenarios and reports identical on a known-good build; `cargo test --lib` still exits 0 without the stack up.

### 16.13 (Stretch, phase 2) Randomized differential fuzzer
- [ ] A seeded generator producing random op sequences over the small alphabet (`put`/`mkcol`/`move`/`copy`/`delete`/`propfind`/`proppatch`), replayed identically on both sides, with **failure-shrinking** to a minimal reproducing sequence.
- [ ] Keep seeds deterministic (record the seed in the report) so a failure is reproducible.

**Verify:** N seeded runs produce no divergence on a known-good build; a seeded `nc-dav` bug (as in 16.6) is discovered and shrunk to a small op sequence.

---

## Out of scope (intentional)

- **Linking the server.** `nc-difftest` is strictly black-box (HTTP + SQL + `docker exec`). It never depends on an `nc-*` crate — that is what makes it an independent oracle.
- **Absolute-state comparison.** Only before/after **deltas** are compared; install-time differences (instanceid, auto-id watermarks) are cancelled by construction.
- **Preview byte equality.** libvips output is not byte-stable across runs; preview scenarios compare DB-row **shape**, not pixels (16.11), unless one Imaginary serves both instances.
- **MySQL/MariaDB.** PostgreSQL is the only production target (CLAUDE.md principle 6); the harness reads Postgres at `:8212`.
- **Replacing Behat/litmus/Cypress.** This harness complements protocol suites — it catches the silent-DB-divergence class they cannot; it does not assert protocol conformance.

---

## References

**Implementation plan**
- [`../03-implementation-plan/plan/16-differential-integration-test-harness.md`](../03-implementation-plan/plan/16-differential-integration-test-harness.md) — full design: canonicalization algorithm, column taxonomy, natural keys, diff/self-check/skip sets, scenario list, quiescing, risks. *(Updated to the `oracle`-clone infrastructure — see that doc's Key facts.)*

**Constitutions**
- [`../../CLAUDE.md`](../../CLAUDE.md) — principles 1/3/4 (behavioral identity; verify against PHP + live schema; match PHP at interop boundaries), principle 6 (PostgreSQL first-class), hygiene rules 3/4/7 (hidden framework side effects; adversarially verify equivalence; read-ops-that-write).

**Infrastructure (verified in-repo)**
- `docker/php84/Dockerfile` — SUT image (Rust `nc-server` on `:80`, no nginx); `docker/bin/bootstrap.sh:239` (DB name from `VIRTUAL_HOST`), `:451` (unconditional `nc-server` start), `:331` (`background:cron`).
- `docker/nginx/my_proxy.conf` — existing `:9090` pure-PHP vhost (the template for the oracle's `:9091`); `docker/configs/php-fpm-tcp.conf` — php-fpm TCP pool `[tcp]` on `:9000`.
- `docker-compose.yml` — `nextcloud` (the clone source), `database-pgsql` (`:8212`, `postgres/postgres`), `proxy`; `.env` — `COMPOSE_PROJECT_NAME=master`, `SQL=pgsql`, `PHP_VERSION=84`, `PORTBASE=821`.
- `core-rs/Cargo.toml` — workspace members + reusable deps.

**Related phases** (behaviors this harness gates)
- [`phase-4.md`](phase-4.md) / [`phase-5.md`](phase-5.md) — DAV files tree + upload flows (native write paths).
- [`phase-9.md`](phase-9.md) / [`phase-10.md`](phase-10.md) — cross-cutting filesystem concerns + PHP-parity remediation.
- [`phase-11.md`](phase-11.md) — native previews (`oc_previews` row shape, 16.11).
- [`phase-8.md`](phase-8.md) — load harness (shares the docker bring-up; distinct purpose).

---

## Changes

*(empty — record here what was tried, reverted and why, root causes, superseded analyses, per the documentation conventions.)*
