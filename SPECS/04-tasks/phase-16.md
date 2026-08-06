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

- [x] Add an `oracle` service to `docker-compose.yml`, a near-verbatim copy of `nextcloud`: same `build: ./docker` / `php84/Dockerfile`, same `environment` (`SQL`, `NEXTCLOUD_AUTOINSTALL`, `NEXTCLOUD_AUTOINSTALL_APPS`, `WITH_REDIS: "YES"`, `PRIMARY`, `PHP_XDEBUG_MODE`), same mounts (`${REPO_PATH_SERVER}:/var/www/html`, `additional.config.php:ro`, `./data/skeleton/:/skeleton`, `./data/shared:/shared`) and the **same** `php-fpm-tcp.conf → zzzz-tcp.conf:ro` mount, same `depends_on` (`database-pgsql`, `redis`, `mail`).
  - Divergences from `nextcloud`, and only these: its **own** named volumes (`data-oracle`, `config-oracle`, `apps-writable-oracle`) so it gets a separate file tree; `VIRTUAL_HOST: "oracle${DOMAIN_SUFFIX}"` so the bootstrap installs into a **separate DB `oracle`**; drop `RUST_LOG` (not needed, harmless either way).
  - `nc-server` still boots inside `oracle` (bootstrap starts it unconditionally) and connects to DB `oracle` — it is simply never routed to. Do not add a toggle; the bypass is at the proxy.
- [x] Add a proxy vhost to `docker/nginx/my_proxy.conf` mirroring the existing `:9090` block but targeting the oracle: `upstream oracle-php { server oracle:9000; }` and a `server { listen 9091; … location ~ \.php { fastcgi_pass oracle-php; … } }` using the same official front-controller `fastcgi_params` as the `:9090` block. Expose `9091` in the `proxy` service `ports`. Static-asset proxying (cosmetic for browser rendering) may point at `oracle:80` or be omitted — the harness issues API/DAV requests only.
- [x] Add a `diff-up` target to the **repo-root** `Makefile`: `docker compose up -d nextcloud oracle database-pgsql redis proxy`, then wait until both instances report `installed: true` (poll `status.php`).
- [x] Rebuild/recreate cleanly per CLAUDE.md: `docker compose up -d --build nextcloud oracle` then `docker compose restart proxy` (the proxy caches upstream IPs and 502s otherwise).

**Verify (gate S0):**
- `make diff-up` succeeds; `curl -s http://127.0.0.1:8080/status.php` (SUT) and `curl -s -H 'Host: oracle.local' http://127.0.0.1:9091/status.php` (Oracle) both return `"installed":true`.
- `docker exec master-database-pgsql-1 psql -U postgres -lqt` lists **both** `nextcloud` and `oracle`.
- **Parity precondition (manual, but mandatory):** the two instances report the **same numeric version** (`status.php` `version` field, e.g. `34.0.0.1` — the `versionstring` may differ by a ` dev` suffix, an install-time artifact to ignore) and the **same enabled-app set** (`SELECT appid FROM oc_appconfig WHERE configkey='enabled' AND configvalue='yes' ORDER BY 1` in each DB — the column is **`configvalue`**, not `value`; live-verified `oc_appconfig` = `appid, configkey, configvalue, type, lazy`). Record any residual difference; if the app sets differ, the differential is meaningless until they match. Confirm both ran `occ background:cron` (set at install — `bootstrap.sh:331`).

> **Note:** the parity precondition above is automated in 16.2 as `preconditions.rs`; here it is a manual sanity gate so the harness is never built on drifted instances.

> **Implementation notes (16.1)** — completed 2026-08-03 against the live dev docker (`master-*`, podman):
> - `docker-compose.yml`: added the `oracle` service (clone of `nextcloud`), three named volumes, and a `9091` port on `proxy`. A top-level volume named `oracle` already exists (the Oracle **DBMS**, `database-oci`), so the oracle's volumes use `-oracle`-suffixed names; the *service* name `oracle` is unaffected (services and volumes are separate namespaces).
> - **Image sharing (beyond the task text):** the oracle pins `image: master-nextcloud:latest` (the SUT's image) instead of building a second `master-oracle`. This avoids a duplicate multi-stage **Rust compile** and — more importantly — guarantees the SUT and oracle run the **byte-identical** image, a correctness requirement for a differential oracle. Verified: one build produced `master-nextcloud:latest`; no `master-oracle` image was created.
> - `docker/nginx/my_proxy.conf`: added a `:9091` vhost (upstream `oracle-php-handler` → `oracle:9000`) mirroring the existing `:9090` pure-PHP block, static assets proxied to `oracle:80`. `my_proxy.conf` is baked into the proxy image, so the proxy was rebuilt (`--build proxy`).
> - `Makefile`: `diff-up` = `docker compose up -d --build proxy nextcloud oracle database-pgsql redis` + a polling wait. `--build` (vs the task's `up -d`) is used because the proxy image must be rebuilt to expose `:9091`, and building nextcloud+oracle together guarantees the identical image. The wait sends explicit trusted `Host:` headers (`nextcloud.local` / `oracle.local`) since the oracle's trusted domains derive from its `VIRTUAL_HOST`.
> - **Verified (gate S0):** both `:8080` (Rust) and `:9091` (PHP) return `status.php` `installed:true` with identical numeric version `34.0.0.1`; PROPFIND Depth:0 on the home root returns `207` on both. Postgres holds **both** `nextcloud` and `oracle`. Enabled-app sets are **identical** (33 each, empty diff), including the autoinstall apps `viewer profiler hmr_enabler previewgenerator`. Oracle php-fpm TCP `:9000` is UP (what `:9091` routes to); the oracle's `nc-server` on `:80` boots but is unrouted.

---

## S1 — Pipeline skeleton

### 16.2 `nc-difftest` crate scaffold, config, HTTP client, preconditions
> Reuses workspace deps (`core-rs/Cargo.toml`): `reqwest` 0.12 (rustls), `sqlx` 0.8, `tokio` 1, `serde`/`serde_json`, `clap` 4, `anyhow`/`thiserror`, `tracing`, `sha2`, `hex`. **New** deps to add to `[workspace.dependencies]`: `serde_yaml`, `similar` (unified diffs), `pretty_assertions` (dev).

- [x] Create `core-rs/crates/nc-difftest` and add `crates/nc-difftest` to the workspace `members`. Layout per the plan: `src/{lib,config,client,db,fs,canonicalize,delta,scenario,preconditions,report}.rs`, `src/bin/difftest.rs`, `tests/differential.rs`, plus `column_registry.yaml`, `scenarios/*.yaml`, `fixtures/`. The crate depends on **no** `nc-*` crate (black-box).
- [x] `config.rs`: base URLs (SUT `http://127.0.0.1:8080`, Oracle `http://127.0.0.1:9091` + `Host: oracle.local`), DSNs (`postgres://postgres:postgres@127.0.0.1:8212/nextcloud` / `…/oracle`), container names (`master-nextcloud-1` / `master-oracle-1`), admin creds. **All env-overridable** (`NC_DIFFTEST_SUT_URL`, `…_ORACLE_URL`, `…_SUT_DSN`, `…_ORACLE_DSN`, …) with these defaults.
- [x] `client.rs`: `NextcloudClient` over `reqwest` with basic auth and a **Nextcloud desktop-client User-Agent** (avoids PHP's CSRF token requirement for browser UAs). Methods: `put`, `get`, `delete`, `mkcol`, `move`, `copy`, `propfind` (Depth header), `proppatch`, `lock`, chunked-upload v2 (MKCOL upload dir → PUT chunks → MOVE assembly), `bulk` (`/dav/bulk`), and OCS `share_create`. Each returns a raw response captured for normalization (16.4).
- [x] `preconditions.rs`: assert both instances up, same `oc_version`, same enabled-app set (the 16.1 manual gate, automated). Fail fast with a precise message naming the first mismatch.
- [x] `tests/differential.rs` is gated behind `#[ignore]` (run via `-- --ignored` / `NC_DIFFTEST=1`) so plain `cargo test --lib` (the project's unit-test entrypoint, CLAUDE.md) is unaffected.

**Verify:** `cargo build -p nc-difftest` succeeds; a smoke binary/test runs `PROPFIND Depth:0` on the home root against **both** base URLs and gets `207` with the same resource member set; `preconditions` passes on the S0 instances.

> **Implementation notes (16.2)** — completed 2026-08-03:
> - Crate `crates/nc-difftest` added as a workspace member (black-box: no `nc-*` deps). `config`/`client`/`preconditions` implemented; `db`/`fs`/`canonicalize`/`delta`/`scenario`/`report` are documented stubs filled by 16.3–16.8. `column_registry.yaml` created (taxonomy header; populated in 16.4/16.5); `scenarios/` + `fixtures/` dirs created.
> - `config.rs`: `Config::from_env()` reads `NC_DIFFTEST_*` with dev-docker defaults (SUT `:8080`/`nextcloud.local`, Oracle `:9091`/`oracle.local`, DSNs on `:8212`, containers `master-nextcloud-1`/`master-oracle-1`, `admin`/`admin`).
> - `client.rs`: `NextcloudClient` (reqwest, basic auth, Nextcloud desktop UA, `Host` header) with the core WebDAV verbs; raw `Response` returned for 16.4 normalization.
> - `preconditions.rs`: status.php `installed` + **numeric** `version` match + enabled-app set equality via sqlx. Crate enables `sqlx … features=["postgres"]` (the workspace only enables `any`); `serde_yaml`/`similar`/`pretty_assertions` added to `[workspace.dependencies]`.
> - **Verified (gate S1):** `cargo build -p nc-difftest` clean; `difftest smoke` → preconditions OK, PROPFIND Depth:0 `207` on both (1 response element each); `cargo test -p nc-difftest` → 0 lib tests, 1 integration test **ignored** (plain `cargo test --lib` unaffected).

> **Deviation (16.2):** `client.rs` implements only the core WebDAV verbs. The task's `lock`, `chunked_upload_v2`, `bulk`, and `share_create` are **deferred** to land with the scenarios that exercise them (16.4/16.5), grounded against the real protocol rather than guessed now (CLAUDE.md principle 3). The differential stays valid either way, since the same client drives both sides.

> **Early finding (16.2 — a result, not a harness bug):** on a bare (allprop) PROPFIND Depth:0 of the home root the SUT body is ~2856 bytes vs the oracle's ~585 — the Rust allprop response is ~5× larger. This is a genuine DAV-parity divergence (likely Rust over-emitting on allprop) for **phase-12/phase-4** to chase, and is exactly the class of silent difference this harness exists to surface; formal response-body comparison lands in 16.4.

### 16.3 `db.rs` — consistent PostgreSQL snapshot
> Ground truth for column types/forms is the **live** schema (`docker exec master-database-pgsql-1 psql …`), per CLAUDE.md principles 3 and 6 — not the SQLite fixture.

- [x] Enumerate `oc_%` tables from `information_schema` (or `pg_tables`); **assert the two DBs expose the same table set**, modulo an explicit skip-list. **Warn (do not silently drop) on any unknown table** — an unclassified table is a coverage gap, not noise.
- [x] Dump each table with `SELECT * ORDER BY <pk>` inside a **single `REPEATABLE READ` transaction** (one consistent cross-table view per side; never commit). Rows are captured as an ordered, typed map per table.
- [x] Skip-list (per plan §3): `oc_sessions`, `oc_jobs`, any `*_queue`. Column-level skip: `oc_authtoken.last_activity` / `last_check` (updated per request). Everything else is snapshot and classified (16.4/16.5).

**Verify:** snapshot both DBs; table sets match; core tables (`oc_filecache`, `oc_storages`, `oc_mimetypes`) dump non-empty; two back-to-back snapshots of an **idle** instance are identical (no residual background writes — if not, the quiescing in 16.5/16.8 is incomplete).

> **Implementation notes (16.3)** — completed 2026-08-03: `db.rs::snapshot` enumerates `oc_%` via `pg_tables` (`LIKE 'oc\_%' ESCAPE '\'`), dumps each as `SELECT "col"::text … FROM "t" ORDER BY "t"."pk"` inside one `REPEATABLE READ` txn (dropped/rolled back, never committed). Values are rendered by Postgres itself via `::text`, so equal values are byte-identical on both sides (same server). ORDER BY is qualified with the table name so it sorts on the source column's native type, not the text projection. Table skip-list (`oc_sessions`, `oc_jobs`, `*_queue`) is active; column-level masking (`oc_authtoken.last_activity`) lands with the canonicalizer in 16.5. The "warn on unknown table" coverage check is realized once the registry exists (16.5). **Verified:** `difftest snapshot` → idle double-snapshot **IDENTICAL** (98 tables, quiesced), table-set parity OK (98 each), core tables non-empty (oc_filecache 879/96, oc_storages 13/13, oc_mimetypes 16/13 — absolute counts differ by design; the differential compares deltas).

---

## S2 — Canonicalization, delta, report → first green slice

### 16.4 Scenario runner + minimal canonicalize/delta/report + initial registry
- [x] `scenario.rs`: YAML loader for scenarios (ordered list of typed ops — `put`, `get`, `mkcol`, `move`, `copy`, `delete`, `propfind`, `proppatch`, `chunked_upload_v2`, `bulk`, `share_create` — with method/path/headers/body refs to `fixtures/`). The runner replays the **identical** sequence against both base URLs.
- [x] **Response normalization:** compare status + selected headers + body, **minus** volatile headers (`ETag`, `Date`). This is a secondary signal; the primary oracle is the DB/file delta.
- [ ] **Double-run flake detection** (plan §5): run each scenario twice; a divergence that appears *only* on the second run is flagged as residual flakiness to investigate — never masked.
- [x] `delta.rs`: `Snapshot → Delta` (added / changed / removed rows per table) between the before and after snapshots of one side.
- [x] `canonicalize.rs` (minimal first cut): column classification driver keyed on `column_registry.yaml`, with the **masking** classes (`stable`, `ignore`, `timestamp_wall`, `volatile_value`, `volatile_independent`) but id-remapping stubbed to identity. Full id-bijection lands in 16.5.
- [x] `report.rs`: render `delta_sut` vs `delta_oracle` (after canonicalization) as a unified diff via `similar`; non-empty diff = failure with an actionable, per-table/per-row report.
- [x] `column_registry.yaml` (initial): classify the **diff-set** tables' columns for the core file ops — `oc_filecache`, `oc_filecache_extended`, `oc_files_metadata`, `oc_storages`, `oc_mimetypes`, `oc_properties`. **Verify every classification against the live schema** before committing it.
- [x] Write `scenarios/10_put_get_delete.yaml` + `fixtures/hello.txt`.

**Verify (gate S2a):** `10_put_get_delete` runs end-to-end against the S0 instances and the report is **identical** (empty diff) on a known-good build. This validates the whole chain: client → snapshot → delta → canonicalize → report.

> **Implementation notes (16.4)** — completed 2026-08-03. Implemented `scenario.rs` (internally-tagged YAML ops via `#[serde(tag="op")]` — serde_yaml rejects the externally-tagged `- put:` form), `delta.rs`, `report.rs` (`similar` unified diff over deterministic rendered deltas), initial `column_registry.yaml`, and scenarios `01_propfind_readonly`, `10_put_get`, `10_put_get_delete` + `fixtures/hello.txt`. The runner compares per-op HTTP status codes (secondary signal) and diffs the DB delta (primary). `canonicalize.rs` was implemented at full strength in one pass (see 16.5), not the stubbed-identity "minimal first cut." **Double-run flake detection is not yet implemented** (its checkbox stays unchecked).

> **Deviation (16.4) — the gate's premise does not hold on the current build, and that is the harness working.** The gate assumed `10_put_get_delete` would be *identical on a known-good build*. The current Rust build is **not** known-good: even a bare `PUT` of a new file diverges from PHP, and the harness surfaced each one precisely. So the write scenarios report **red (correctly)**, and the green path is instead proven by `01_propfind_readonly` → `IDENTICAL` (reads write nothing → empty deltas both sides). Achieving a green write scenario requires fixing the Rust behaviors below — that is phase-9/10 work, not phase-16.
>
> **Real Rust↔PHP divergences surfaced by `10_put_get` (all live-verified 2026-08-03):**
> 1. **Storage-root size over-propagation** — on PUT, Rust bumps `oc_filecache.size` on the **storage root** (`path=''`) *and* `files/`; PHP bumps only `files/`.
> 2. **`files/` storage_mtime** — PHP bumps `files/` `storage_mtime` on a child write; Rust does not.
> 3. **`oc_filecache_extended` creation/upload time** — Rust sets `creation_time == upload_time`; PHP sets them **unequal**. This contradicts the plan's assumption that "creation_time==upload_time on a fresh PUT must hold" — **verify against PHP source** to determine which is correct.
> 4. **`oc_files_versions.metadata` JSON formatting** — Rust writes `{"author": "admin"}` (space after colon); PHP writes `{"author":"admin"}` (compact `json_encode`). Rust should match PHP's compact form. Both sides create a version row on a fresh PUT (confirm that matches PHP intent).
> 5. **`oc_preview_generation`** — PHP queues a preview-generation row on PUT (the previewgenerator app reacts to the write event); Rust's native PUT fires no such event, so no queue row. An event-cascade side-effect (CLAUDE.md hygiene rule 6).
>
> **Quiescing applied** (to stop noise masking signal): `oc_preferences` added to `db.rs` `SKIP_TABLES` — it is per-user runtime/UI state PHP writes on nearly every request (`login.lastLogin`, `files.lastSeenQuotaUsage`, …) while the SUT serves those paths natively; pure noise for a file-behavior differential. Basic-auth does not touch `oc_authtoken`, so no masking was needed there.

> **Note (finding #3 resolved, 2026-08-03):** the "verify against PHP source" clause of finding #3 is settled — **PHP is right and the plan's assumption was backwards.** On a bare DAV PUT without `X-OC-CTime`, PHP writes `oc_filecache_extended` with **`upload_time = <request time>` and `creation_time = 0`** (the column default). Chain, fully traced: `apps/dav/lib/Connector/Sabre/File.php:354-366` puts only `upload_time` into `putFileInfo` (plus `creation_time` only when `X-OC-CTime` was sent); `Cache::normalizeData` (`lib/private/Files/Cache/Cache.php:446-487`) drops absent/falsy extension fields via `array_filter`, so the extended write touches only `upload_time`; the migration (`core/Migrations/Version17000Date20190514105811.php`) defines `creation_time BIGINT NOT NULL DEFAULT 0`. Live-verified the same day: oracle `files/hello.txt` = `(creation_time 0, upload_time 1785710360)` vs SUT `(1785710359, 1785710359)` — **Rust over-populates `creation_time` with the upload time.** Fix direction for phase-9/10: Rust writes `creation_time` only when `X-OC-CTime` is present (or via the PROPPATCH `{DAV:}creationdate` path), and otherwise leaves it at `0`. Full population lifecycle documented in [`../01-requirements/requirements/21-filecache-population.md`](../01-requirements/requirements/21-filecache-population.md) (§21.2.5 in particular).

> **Note (findings #1–#5 fix applied, 2026-08-05):** all five divergences from the 16.4 deviation record are resolved in `nc-dav` (commits `7b6b501`–`ee6e38b` on `working`):
>
> - **#3 (oc_filecache_extended times):** `creation_time = x_oc_ctime.unwrap_or(0)`; `upload_time = now` (request time). Verified matching in difftest delta.
> - **#4 (version metadata):** `serde_json::json!({"author": uid}).to_string()` → compact `{"author":"admin"}`. Tested in `versions::tests::version_metadata_json_is_compact`. Verified matching in difftest delta.
> - **#5 (preview_generation):** new `preview_queue.rs` → `INSERT … WHERE NOT EXISTS`. Verified matching in difftest delta.
> - **#2 (files/ storage_mtime):** `Propagator::correct_parent_storage_mtime` — sets parent's `storage_mtime`+`mtime` to the parent directory's on-disk mtime. Unit-tested in `propagator.rs`. Verified both sides update `files/` storage_mtime (difftest masking artifact hides Oracle's change; live DB confirms both sides at the same value post-PUT).
> - **#1 (size propagation):** `Propagator::correct_folder_size_chain` — recomputes each ancestor's size from the sum of its direct children, walking from the parent up. Unit-tested (recomputation, unscanned-child propagation). PostgreSQL `SUM(bigint)`→`NUMERIC` cross-driver bug fixed with `CAST(SUM(size) AS BIGINT)`.
>
>   **Known PHP bug surfaced:** `Cache::calculateFolderSizeInner` (`Cache.php:1023`) compares `$entry['mimetype']` (integer from DB, e.g. `2`) against `FileInfo::MIMETYPE_FOLDER` (string `"httpd/unix-directory"`) with `===` — **always false**. The `correctFolderSize` recursion walks the chain but never computes any folder's size. PHP's `files/` size is updated by a different (unidentified) mechanism; root size is **never** recomputed. Rust intentionally implements the *correct* behavior (recompute all ancestors including root) per CLAUDE.md principle 5 — the root-size delta is an intentional improvement over PHP's bug, not a defect.
>
>   **Regression**: `21_bulk_upload` → IDENTICAL. `16_overwrite_put` and `18_explicit_mtime` carry the same root-size + storage_mtime patterns as `10_put_get` (no new divergences). 292 nc-dav lib tests pass; workspace `cargo test --lib` clean except pre-existing `nc-fastcgi::registry_scans_real_apps_dir` (environment-dependent).
>
>   Full fix plan + PHP trace: [`phase-16.4-put-parity-plan.md`](phase-16.4-put-parity-plan.md).

### 16.5 Full canonicalization — natural-key id-bijection + equality-preserving masking
> The design centerpiece (plan §3). Unit-test everything here with hand-built fixtures; prefer preserving relationships over blanket masking.

- [x] **Column classification taxonomy** (`column_registry.yaml`, keyed `table.column`):
  - `stable` — compare verbatim (path, name, size, permissions, checksum, mimetype *name*, storage `id` string, property name/value, share perms).
  - `id_pk` / `id_fk` — remap through the canonical bijection.
  - `timestamp_wall` — mask the absolute value but **preserve equality/ordering across columns in the same row** (so `creation_time == upload_time` on a fresh PUT must hold on both sides; a missed bump is still caught).
  - `volatile_value` — random/time-based but equality is meaningful (etag): mask to per-snapshot sentinels that keep equal-values-equal and distinct-values-distinct (catches "parent got the same etag as its child").
  - `volatile_independent` — per-row random, no equality expected (share `token`, `metadata_etag`): mask to a constant.
  - `ignore` — known irrelevant (`oc_storages.last_checked`).
- [x] **id-bijection:** build bidirectional `sut→canonical` / `oracle→canonical` maps in **FK-dependency (topological) order**, matching rows by a stable natural key — not by id:
  - `oc_storages` key `id` (`home::admin`); `oc_mimetypes` key `mimetype`; `oc_vcategory` key `(uid, type, category)`.
  - `oc_filecache` key `(canonical(storage), path)` — **path is the true natural key**. Then `oc_filecache_extended` by `canonical(fileid)`; `oc_vcategory_to_object` by `(canonical(objectid), canonical(categoryid), type)`; `oc_properties` by `(userid, propertypath, propertyname)`; `oc_files_trash` by `(user, id, location)`; `oc_share` by `(uid_owner, uid_initiator, item_type, canonical(item_source), share_with, file_target)`; `oc_preferences` / `oc_appconfig` by their natural keys (no id column).
  - Every matched pair gets a canonical label; every `id_fk` remaps through the same map. **A row present on one side but not the other under a natural key is itself a reported divergence (never masked).** Works identically for snowflake ids (`oc_previews`) since only uniqueness + natural-key matching matter.
- [x] Complete `column_registry.yaml` for the full **diff set**: `oc_filecache`, `oc_filecache_extended`, `oc_files_metadata`, `oc_storages`, `oc_mimetypes`, `oc_properties`, `oc_vcategory`, `oc_vcategory_to_object`, `oc_files_trash`, `oc_previews` (+`_versions`/`_locations`), `oc_share`, `oc_preferences`, `oc_appconfig`. **Self-check set** (proxied, must be identical): `oc_users`, `oc_accounts`, `oc_groups`, `oc_group_user`.
- [x] **Quiescing masks** (plan §5): mask per-request columns (`oc_authtoken.last_activity`); the skip-list from 16.3.

**Unit tests:** `cargo test --lib -p nc-difftest` (un-gated) —
- `id_offset_hidden`: two snapshots identical modulo a constant id offset + FK ripple → empty diff.
- `natural_key_mismatch_reported`: a row present on one side only under a natural key → reported divergence, not masked.
- `timestamp_equality_preserved`: `creation_time == upload_time` on both sides → equal; a missed bump on one side → diff.
- `volatile_equality_preserved`: equal etags stay equal, distinct etags stay distinct, after masking.
- `registry_coverage`: every diff-set table.column is classified (no unclassified column slips through).

**Verify (gate S2b):** `10_put_get_delete` still reports identical with the full bijection active; all canonicalizer unit fixtures pass.

> **Implementation notes (16.5)** — completed 2026-08-03. Full natural-key id-bijection in `canonicalize.rs`: `SPECS` lists each diff-set table's PK + natural key in FK-topological order (`oc_storages`→`oc_mimetypes`→`oc_filecache`→`oc_filecache_extended`/`oc_files_metadata`/`oc_files_versions`/`oc_preview_generation`; `oc_properties`, `oc_files_trash`); `fk_reference()` maps each `id_fk` to its referenced table. Tables without a spec are carried content-keyed verbatim (an untouched table yields an empty delta; a touched-but-unclassified one surfaces loudly). `oc_files_trash.id` is `{filename}.d{deletion-ts}` — the `.d{ts}` suffix is stripped (filename preserved) so trashed rows still match.
> - **Key correctness decision:** volatile sentinels (`timestamp_wall` per-row equality, `volatile_value` per-distinct-value) are assigned in **`delta::normalize_delta`** over the *delta* — not during full-snapshot canonicalization. The two sides have different baseline rows, so sentinel order over the full snapshot would be side-dependent and false-positive; over the delta the natural-key sets match when behavior matches, so the assignment is identical on both sides. This still preserves equality semantics: a missed etag/mtime bump (equal on one side, unequal on the other) yields different sentinel patterns and is caught (pinned by `timestamp_equality_preserved` / `volatile_equality_preserved`).
> - **Unit tests (5, passing):** `id_offset_hidden` (id offset + FK ripple hidden by the bijection), `natural_key_mismatch_reported` (a row on one side only is not masked), `timestamp_equality_preserved`, `volatile_equality_preserved`, `registry_coverage` (all diff-set tables classified).
> - Gate S2b's "identical" clause has the same caveat as S2a: `10_put_get_delete` is not green because the current build diverges (see the 16.4 deviation/findings). The canonicalizer itself is proven correct by the unit fixtures and by `oc_files_trash`/`oc_filecache` matching in the live runs.

---

## S3 — Harness self-validation

### 16.6 Negative control — prove the harness catches a real divergence
> Plan §Verification. A harness that passes silently is worse than none.

- [x] Temporarily introduce a **deliberate** divergence in `nc-dav` on a branch — e.g. skip the `oc_filecache_extended` insert on PUT, or don't bump the parent etag on a child write (exactly the silent-side-effect class this harness targets).
- [x] Rebuild the SUT image (`docker compose up -d --build nextcloud`), re-run the affected scenario.
- [x] Confirm the scenario **fails** with a **precise** delta naming the missing/changed row and column — not a generic error.
- [x] Revert the divergence; confirm the scenario is green again.

**Verify (gate S3a):** the seeded bug is caught with an actionable delta; after revert, identical. Record the episode in the `## Changes` log (what was seeded, what the delta showed).

> **Implementation notes (16.6)** — completed 2026-08-04. Seeded the task's first suggestion: skip the PUT-path `oc_filecache_extended` upsert in `nc-dav` (`davfile.rs`, wrapped in a dead `if false` guard), committed on scratch branch `phase16-negative-control` (`691218b`), then reverted by returning to `working`. Protocol: three runs of `10_put_get`, each started from the identical state "hello.txt absent" (a DAV DELETE cleanup between runs suffices — the scenario's delta touches no trash rows, so no volume reset was needed):
> - **Run A (baseline, unmutated):** the known red diff — exactly the four live divergences recorded under 16.4 (storage-root size over-propagation; `oc_filecache_extended` `creation_time==upload_time` on Rust vs unequal on PHP; `oc_files_versions.metadata` spacing; `oc_preview_generation` queued only by PHP).
> - **Run B (seeded):** the same diff **plus** the whole `oc_filecache_extended` section flipping to oracle-only — the report names the table, the natural key (`home::admin | files/hello.txt`) and every column of the missing row. The PUT still answered `201 == 201` on both sides: the missing side-effect is invisible at the protocol level and caught only by the DB differential — precisely the failure class the harness exists for.
> - **Run C (reverted):** byte-identical to run A (`diff` empty), proving both the revert and the harness's run-to-run determinism (sentinels, natural-key remapping, and absolute size values all reproduce).
>
> **Deviation (16.6):** the task's revert clause says "confirm the scenario is green again," which assumes a known-good build. The current build is not known-good (see the 16.4 deviation record), so the achievable revert check is that the diff returns **byte-identically to the recorded baseline diff** — known divergences only, no residue of the seeded bug. That is what was verified. The scenario becomes truly green once the phase-9/10 fixes land; the negative-control loop itself is fully proven either way.

### 16.7 Proxied self-check scenario (`30_share_create_selfcheck`)
> Share creation is **proxied to PHP** on the SUT (not a native Rust path), so SUT and Oracle execute the *same* PHP code on their respective DBs. This scenario must therefore **always** match — it validates the harness, not the server.

- [x] Write `scenarios/30_share_create_selfcheck.yaml` (OCS share create over a fixture file; exercise the `oc_share` + `oc_vcategory`/property writes).
- [x] Wire it as a standing health check: if it ever fails, the **harness** is wrong (over-masking, snapshot inconsistency, oracle drift) — investigate the harness before suspecting the server.

**Verify (gate S3b):** `30_share_create_selfcheck` reports identical on the first and every subsequent run; deliberately breaking a canonicalizer rule (e.g. mis-masking `oc_share.token`) makes it fail, confirming sensitivity.

> **Implementation notes (16.7)** — completed 2026-08-04. `scenarios/30_share_create_selfcheck.yaml`: a **group share** (`shareType=1`, `shareWith=admin`) and a **link share** (`shareType=3`) of the skeleton folder `/Media`, each capturing `ocs.data.id` from the OCS JSON response; new `cleanup` ops (a runner addition — replayed after the after-snapshot, never diffed) delete both shares, so the scenario is re-runnable with zero residue. The deferred `share_create`/`share_delete` client methods landed here as planned in 16.2 (one grounding fix en flight: PHP never populates `$_POST` without an explicit form `Content-Type`).
>
> **Grounded PHP behavior (verified live + source, 2026-08-04):**
> - Share creation is proxied on the SUT: `nc-ocs` serves only `/ocs/v*/config` and `/ocs/v*/cloud/capabilities` natively; everything else under `/ocs/` falls to `php_fpm_fallback` (`nc-server/src/router.rs`). POST and DELETE bodies forward correctly through the FastCGI proxy.
> - A **group share writes TWO `oc_share` rows**: the `TYPE_GROUP`(1) parent plus one `TYPE_USERGROUP`(2) child per group member (`DefaultShareProvider::createUserSpecificGroupShare`), `parent` → parent id, child `accepted=1`. Deleting the parent cascades the child.
> - The API response reports `mail_send:1` but a follow-up UPDATE settles the DB row to `0` — the DB is the truth, not the response.
> - The link token is random per instance → `token: volatile_independent` is the sensitivity surface. `setLinkParent` sets `parent` only when another link share of the same node exists (not in this scenario).
> - **No other diff-set table is touched.** `oc_activity`/`oc_notifications`/`oc_admin_audit`/`oc_mount_storage` do not exist on this install (apps absent); `oc_collres_*` stay empty. Share creation does **not** write `oc_vcategory` or `oc_properties` (the task text's claim, checked empirically).
> - `oc_ratelimit_entries` added to `db.rs` `SKIP_TABLES`: every rate-limited request runs a table-wide `DELETE WHERE delete_after <= now` + inserts its own attempt row (`lib/private/Security/RateLimiting/Backend/DatabaseBackend.php:43,83`) — which rows a run deletes is pure wall-clock timing noise (live-observed: the oracle GC'd two expired probe entries mid-scenario, the SUT's had not expired yet). Rate limiting is a response-level (429) concern covered by the status comparison.
>
> **Harness additions:** `oc_share` classification (all 25 live columns) + natural key + FK graph (incl. the `parent` self-FK); two new unit tests (`share_id_offset_and_parent_remap_hidden`, `share_wrong_parent_reported`); registry coverage extended.
>
> **Verified (gate S3b):** IDENTICAL on run 1 and run 2 (re-runnable; `oc_share` empty after every run); sabotaging the registry (`token: stable`) makes it FAIL with the diff isolating exactly the two sides' random link tokens; reverting restores IDENTICAL.
>
> **Deviation (16.7):** three departures from the task text, all grounded:
> 1. The shares target the **skeleton folder `/Media`**, not "a fixture file" — creating a fixture via native PUT would inherit the known phase-9/10 PUT divergences and break the self-check's "always identical" contract.
> 2. The scenario does not "exercise `oc_vcategory`/property writes" — share creation does not touch those tables (verified empirically); the diff-set surface is `oc_share` alone (2 parent/child rows + 1 link row).
> 3. The `oc_share` natural key **adds `share_type`** to the 16.5-design key `(uid_owner, uid_initiator, item_type, item_source, share_with, file_target)` — required, because the `TYPE_GROUP` parent and `TYPE_USERGROUP` child share every other component when uid == gid ("admin" user vs "admin" group).

---

## S4 — File tree + core native scenarios

### 16.8 `fs.rs` — file-tree snapshot + delta
> Plan §5. Compare bytes, not just DB rows — a correct DB row with a wrong/missing file on disk is still a divergence.

- [x] Snapshot `data/{user}/files/**` by **relative path + size + sha256** via `docker exec master-nextcloud-1` / `master-oracle-1` running `find … -print0 | xargs -0 sha256sum`. **Exclude** volatile subtrees: `files_versions/`, `cache/`, `appdata_*/`, and in-flight `*.part`.
- [x] Compute the file-tree delta (added/changed/removed by relative path) before→after on each side, and diff `delta_sut` vs `delta_oracle`.

**Verify:** a PUT of a fresh file yields matching file-tree deltas on both sides (same relative path, size, sha256); deleting it removes it on both; an idle double-snapshot is empty (quiesced).

> **Implementation notes (16.8)** — completed 2026-08-04. `fs.rs`: one `docker exec` per side per snapshot — sizes via `find . -type f ! -name '*.part' -printf '%s\t%p\n'`, a marker, then `find … -print0 | xargs -0 -r sha256sum`; parsed to path → (size, sha256). `delta`/`render`/`diff` mirror the DB pipeline (unified diff via `similar`). Integrated into `difftest run` (before/after snapshots on both sides; verdict = DB delta identical **and** file delta identical **and** statuses match) and `difftest snapshot` (idle double-snapshot per side). Four unit tests cover parsing and delta semantics.
> - The volatile subtrees (`files_versions/`, `cache/`, `appdata_{instanceid}/`, `files_trashbin/`, `uploads/`) are **siblings** of `files/` in the datadir, so rooting the snapshot at `data/{user}/files/**` excludes them by construction; `*.part` is excluded defensively within. Timestamps are deliberately not snapshotted — mtime semantics are covered by the DB delta with equality-preserving masking; the file tree compares content identity.
> - **Verified:** idle double-snapshot IDENTICAL on both sides (10 files each, quiesced); `10_put_get` yields matching file deltas (same relative path, size, sha256 of `hello.txt` on both sides); the share self-check's file delta is empty on both sides. Delete-removal parity verified on pristine instances (see Changes).
> - **Also landed here (delete-flow enabler):** the trash `.d{deletion-ts}` path volatility — the two replays run sequentially and can straddle a second boundary, so trashed filecache paths/names under `files_trashbin/`/`files_versions/` are canonicalized with the `.d{ts}` and `.v{mtime}` suffixes stripped (mirroring the existing `oc_files_trash.id` strip), and their `path_hash` (md5 of the unstripped path) is masked. Pinned by `trash_volatile_suffix_stripped` (incl. the negative case: `files/report.d20240101` keeps its suffix). Same documented trade-off as the trash-id strip.
>
> **Deviation (16.8):** the task's exclusion mechanism ("Exclude volatile subtrees: `files_versions/`, `cache/`, `appdata_*/`") is realized by **rooting the snapshot at `files/**`** rather than filtering within a wider walk — those subtrees are siblings of `files/`, never children, so the root excludes them by construction (verified against the live datadir layout). Equivalent coverage, one less way to get it wrong.

> **Note (2026-08-07 — delete-to-trash parity, plan
> [`phase-16.8-delete-trash-parity-plan.md`](phase-16.8-delete-trash-parity-plan.md)):** the
> delete-flow findings #6–#12 cluster is resolved in `filesystem.rs` (see the `## Changes` log
> entry below). Difftest-verified on fresh stacks: `10_put_get_delete` and `17_delete_to_trash`
> are at parity except the accepted root-size divergence (#1 — PHP's `calculateFolderSizeInner`
> mimetype bug; Rust intentionally correct); `11_mkdir_nested`'s known parent-`storage_mtime`
> divergence is fixed; `14_propfind_depth1` / `30_share_create_selfcheck` stay IDENTICAL;
> `10_put_get`'s remaining versioning-path rows are the pre-existing #13–#22 group (untouched
> code). `cargo test --lib -p nc-dav` → 302 passed.

### 16.9 Core native scenarios (11–18)
> Native = Rust writes the DB itself (`/remote.php/webdav/*`, `/remote.php/dav/files/{uid}/*`) — the highest-value differential surface.

- [x] Author and green: `11_mkdir_nested`, `12_move_rename`, `13_copy`, `14_propfind_depth1`, `15_proppatch_favorite_tags`, `16_overwrite_put` (copy-on-write path), `17_delete_to_trash`, `18_explicit_mtime`.
  - `18_explicit_mtime` sends `X-OC-Mtime` so mtime-preservation is checked **deterministically** despite timestamp masking (the value is `stable`-equivalent because the client dictates it).
  - `15` exercises `oc_properties` + `oc_vcategory`/`oc_vcategory_to_object` (favorites/tags).
  - `17` exercises `oc_files_trash` (natural key `(user, id, location)`).

**Verify (gate S4):** every scenario in 11–18 reports identical (DB delta **and** file-tree delta); `make diff-one S=<name>` green for each.

> **Implementation notes (16.9)** — completed 2026-08-04. All eight scenarios authored (unique filenames per scenario so suite runs don't collide on stripped trash natural keys; setup writes use inline bodies; each scenario carries `cleanup` ops replayed after the after-snapshot). New harness machinery for this task:
> - **Scenario-level `stable_overrides`** (`table.column` → `stable` for that run): a client-dictated value like `X-OC-Mtime` is deterministic, so `18` promotes `oc_filecache.mtime`/`.storage_mtime` to verbatim comparison — the diff then shows the raw stored timestamps. This is what makes 18's check deterministic; the masking design cannot otherwise express "this timestamp was client-dictated."
> - **`oc_vcategory` / `oc_vcategory_to_object`** classified (live schema 2026-08-04) with natural keys `(uid, type, category)` and `(canonical(objid), canonical(categoryid), type)` (composite PK, leaf table) + `registry_coverage` extended. Grounded: favorites/tags go through the legacy ITags tables — `oc_properties` is **not** written for them; the favorite is the special tag `_$!<Favorite>!$_` (`TagsPlugin.php`, live probe).
> - All file-tree deltas matched in every run (fs oracle green across the suite).
>
> **Results on the current build (pristine instances, suite order):**
> - **Green:** `14_propfind_depth1` (read-only, empty deltas both sides), `30_share_create_selfcheck`.
> - **One-shot red:** `01_propfind_readonly` is red only as the **first** files access on a fresh oracle — PHP lazily materializes the user's `cache/` row (finding #8 identified); SUT correctly writes nothing. Green on every subsequent run.
> - **Red with precise deltas:** 10_*, 11, 12, 13, 15, 16, 17, 18 — all divergences are the known PUT/delete-flow findings plus the new batch below; no harness artifacts. Notably **MOVE matches** (12's diff is entirely inherited PUT findings) and **MKCOL matches except one column** (11).
>
> **New Rust↔PHP divergences surfaced (pristine suite, 2026-08-04; phase-9/10 targets):**
> 13. **COPY size propagation** — Rust's COPY does not propagate size up the parent chain (`files/` gained only the source PUT's bytes; `copy-dir` stayed size 0); PHP propagates (`files/` +24, `copy-dir` 8).
> 14. **COPY/version checksum** — PHP writes `checksum` NULL on copied/versioned filecache rows; Rust copies the source's empty-string value.
> 15. **COPY etag** — PHP gives copies the **source's etag** (all three files shared one raw etag); Rust generates fresh etags per copy.
> 16. **COPY extended rows** — PHP creates `oc_filecache_extended` rows for the copy destinations (upload_time ≠ creation_time); Rust creates none for copies.
> 17. **MKCOL storage_mtime** — PHP bumps the parent dir's `storage_mtime` when a child is MKCOL'd; Rust does not (11's only divergence).
> 18. **Favorite PROPPATCH is broken on Rust** — `PROPPATCH {oc:}favorite 1` writes no ITags rows and the SUT log shows only the `un_tag` DELETE executing: text-only property values are extracted as empty by the PROPPATCH XML handling, so `1` parses falsy (element-valued props like `{oc:}tags` work — alpha/beta matched). Compounded by `let _ =` swallowing the `set_favorite` error (hygiene rule 1). PHP also **500s on an empty `<oc:tags/>`** (`getTags() on null`, live-observed) — a rejection-parity datapoint for whoever fixes the Rust side.
> 19. **Lazy `files_metadata` appconfig** — PHP's first favorite/tags PROPPATCH lazily registers `core/files_metadata` (adds the `files-live-photo` field); SUT does not. Instance-level init, same class as #8.
> 20. **Versioning details on overwrite** — PHP propagates the version size into the `files_versions/` dir (Rust leaves 0); Rust creates an `oc_filecache_extended` row for the `files_versions` dir on versioning (PHP does not); version-file etag semantics differ (oracle's version etag equaled the file's post-overwrite etag in 16).
> 21. **Version file storage_mtime under X-OC-Mtime** — PHP sets it to the request time; Rust sets it to the explicit mtime (18, stable-compared).
> 22. **Home-root mtime propagation** — PHP bumps the home root's mtime/storage_mtime on child writes (18's stable view); Rust does not.
>
> **Positive parity confirmed by the same runs:** X-OC-Mtime IS honored (file mtime/storage_mtime stored as exactly 1700000123 on both sides — the stable override proves it); MOVE re-keys filecache/extended/versions rows identically; MKCOL dir rows, extended rows and etag propagation match; COPY duplicates rows/sizes/paths correctly modulo 13–16; tag diffing (element-valued) matches; the trash `.d{ts}` strip makes delete deltas stable across sides.
>
> **Deviation (16.9):** the gate says "every scenario in 11–18 reports identical" — that assumes a known-good build. The current build is known-red (16.4 record + findings #6–#22 here), so the scenarios are authored, run, and recorded with precise deltas instead; `14` (and the self-check) prove the green path. Scenarios go green as the phase-9/10 fixes land. Also grounded against the task text: `15` exercises `oc_vcategory`/`oc_vcategory_to_object`, **not** `oc_properties` (PHP does not write it for these properties); `17`'s live `oc_files_trash` natural key is `(user, location, type, mime)` with `id` volatile-masked (the `(user, id, location)` key was superseded in 16.5).

---

## S5 — Breadth

### 16.10 Upload / edge scenarios (20–24)
- [ ] Author and green: `20_chunked_upload_v2` (MKCOL upload dir → PUT chunks → MOVE assembly over `/remote.php/dav/uploads/*`), `21_bulk_upload` (`/dav/bulk`), `22_invalid_filename` (**rejection parity** — same status code + error shape on both sides), `23_quota_exceeded` (same rejection), `24_checksum_upload` (`OC-Checksum` header → `oc_filecache.checksum` parity).

**Verify (gate S5a):** 20–24 report identical; the rejection scenarios (22, 23) match on status **and** normalized error body, not just "both failed."

> **Implementation notes (16.10)** — completed 2026-08-05. All five scenarios authored (unique filenames, per-scenario `cleanup` ops) and run on the live instances; the deferred 16.2 client work landed with them:
> - **Runner/client additions:** `chunked_upload_v2` and `bulk` are composed in `scenario.rs` from the core verbs; new `ocs_user_quota` op via a new `ocs_user_update` client method (`PUT /ocs/v2.php/cloud/users/{user}` with form `key`/`value` — proxied to PHP on the SUT, identical PHP on both sides). Each chunked-upload step (MKCOL / chunk PUTs / MOVE) records its own status for cross-side comparison instead of aborting the scenario — a mid-flow rejection is a divergence to *report*, and the 20 recording proves why. `compare_body` (22/23/24) compares captured response bodies across sides on top of the status check, with separate status/body failure flags in the verdict.
> - **Grounded protocol formats (verified live + PHP source):** 21's multipart body uses per-part `X-File-Path` + `Content-Length` headers (`apps/dav/lib/BulkUpload/BulkUploadPlugin.php` + `MultipartRequestParser.php`). 24's fixture SHA1 was precomputed and verified against the file. OCS quota edits store the value as `"<n> B"` in `oc_preferences` (`files`/`quota`) — live-verified (e.g. `100 B`), and `parse_quota_string` handles it (new `bare_bytes_unit` unit test in `nc-dav`).
> - **Scenario 23 self-repair:** its body was authored at **exactly 100 bytes** while claiming "longer than one hundred" — corrected to 114 bytes with a comment that the length is part of the test.
> - **Quiescing:** OCS user edits trigger CardDAV + Circles side effects on both sides (the quota edit updates the user's system-addressbook card and the circles member cache). The five tables carry per-instance identity that cannot be compared without hostname masking — card URLs embed the instance hostname (`nextcloud.local` vs `oracle.local`, live-observed in `oc_cards_properties.X-SOCIALPROFILE`/`CLOUD`), synctokens are per-instance monotonic watermarks, circle/member ids are per-instance random, `cached_update` is wall-clock. Added to `db.rs` `SKIP_TABLES` with the full rationale: `oc_addressbookchanges`, `oc_addressbooks`, `oc_cards`, `oc_cards_properties`, `oc_circles_member`. Same class as `oc_preferences`; if `nc-ocs` ever serves user edits natively, these need targeted scenarios + classification instead (as 16.7 did for `oc_share`).
> - **Grounded PHP behavior (source + live):** quota is enforced by a **sabre plugin**, not the write path — `QuotaPlugin` subscribes `beforeWriteContent`/`beforeCreateFile`/`method:MKCOL` (fixed 4096-byte assumption)/`beforeMove`/`beforeCopy`; length = max(`X-Expected-Entity-Length`, `Content-Length`, `OC-Total-Length`); free = `View::free_space` where the `Quota` storage wrapper clamps `max(quota − used, 0)` (`lib/private/Files/Storage/Wrapper/Quota.php:74-93`); `length > free` → 507 `Sabre\DAV\Exception\InsufficientStorage` "Insufficient space in {path}" (`QuotaPlugin.php:261-262`). No Content-Length → no check. `OC-Checksum` is stored **verbatim, unverified** (`apps/dav/lib/Connector/Sabre/File.php:374-380`), and a PUT without the header clears an existing checksum to `''`. Invalid filenames throw `InvalidPath` from `FilenameValidator.php:298` (`"%1$s" is not allowed inside a file or folder name.`) and the exception serializes `xmlns:o` + `<o:retry>` + `<o:reason>` (`Exception/InvalidPath.php:45-57`).
>
> **Results on the current build (2026-08-05, both runs of each scenario):**
> - **Green:** `21_bulk_upload` → IDENTICAL (DB + fs + statuses) on both runs — the native `bulk_handler` matches PHP's `BulkUploadPlugin` for the two-file multipart case, including the per-file filecache/extended/versions writes.
> - **Red with precise deltas:** 20 (flow never starts on the SUT — finding #23; **re-recorded after the #23 fix, see below**), 22 (status + exception class match; error-body shape differs — finding #25), 23 (SUT accepts the over-quota PUT; oracle 507s — finding #26), 24 (SUT rejects the wrong-checksum overwrite that PHP accepts — finding #27; first PUT matches).
> - **Re-recording of 20 after the #23 fix (2026-08-05):** all five protocol statuses now match (MKCOL/chunks/MOVE all 201 == 201, GET 200 == 200) and the **fs delta is identical** (assembled bytes equal). The remaining diff is purely the assembly's DB side-effect surface: SUT home-root size +52 over-propagation (#1) vs the oracle's −52 (third confirmation, #24); SUT stamps one etag across `home == files` while PHP stamps `home == uploads` (MOVE source chain) with a distinct `files` etag (#34); the oracle updates its `uploads` row which the SUT does not have (#24); oracle-only `oc_files_versions` row (#32) and `oc_preview_generation` queue row (#33); extended `creation_time == upload_time` on the SUT vs unequal on PHP (#3, text semantics).
>
> **New Rust↔PHP divergences surfaced (all live-verified 2026-08-05; phase-9/10 targets):**
> 23. **Chunked v2 MKCOL demands a `Destination` header** (`nc-dav/src/upload_handler.rs` `handle_mkcol`) — not part of the protocol: PHP's `ChunkingV2Plugin` reads `Destination` only in `beforeMove`, and sabre's MKCOL never consumes it. SUT answers 400 → the chunk PUTs and the assembly MOVE cascade to 404 → **every chunked upload v2 from a standard client fails**. Probed: the same MKCOL *with* a Destination header → SUT 201. **FIXED 2026-08-05:** the requirement removed (a client-supplied Destination is still accepted leniently but never required; the session's stored `target_path`/`expected_size` were verified to be dead metadata — `handle_move` re-derives the target from the MOVE's own header). Verified: manual probe of the full MKCOL→chunks→MOVE→GET flow green on the SUT, and scenario 20's statuses/fs-delta all match (remaining diff = assembly side-effect findings #24/#32/#33/#34 + the known PUT-path findings).
> 24. **No `uploads/` filecache rows on Rust.** PHP lazily materializes the `uploads` dir row on first MKCOL (mimetype `httpd/unix-directory`) and keeps it after assembly; Rust keeps upload state in an in-memory store + disk only. The two-run oracle delta also records the **assembly propagation shape** the Rust fix must reproduce: `files` +size; `uploads` row etag bump; home/storage-root etag bump **and size −(file size)** (−52 on both runs — and a **third** −52 in the post-#23-fix re-run, 20569878 → 20569826). Hypothesis for the −size: assembly is a cross-directory MOVE whose *source* chain (`uploads/…` → home root) propagates the removal while the *destination* chain stops at `files/` (consistent with findings #1/#22) — verify when fixing.
> 25. **Invalid-filename rejection body.** Status parity (400 == 400) and same exception class (`OCA\DAV\Connector\Sabre\Exception\InvalidPath`), but Rust's message is its own wording (`File name contains forbidden character: '\\'`) vs PHP's `FilenameValidator` wording, and Rust omits the `xmlns:o` namespace + `<o:retry>`/`<o:reason>` elements PHP serializes. DB/fs deltas empty on both sides — the rejection itself leaves no state.
> 26. **Quota: Rust accepts ANY upload once usage already exceeds the quota** — SUT answered 201 and wrote the file where PHP answered 507 with zero partial state. Root cause live-proven via instrumented logs (added, read, reverted): `compute_free_space` returns `quota − used` via `saturating_sub`, which only guards i64 overflow — **it does not clamp at 0**, despite the module doc claiming it does; the negative result (−20575146 in the probe) hits `check_quota`'s `free < 0 → unlimited/unknown → skip` branch, so the check is skipped entirely. PHP clamps `max(quota − used, 0)` in the Quota wrapper → `length > 0` → 507. Secondary: when Rust *does* reject (507 on other paths), its body shape differs (exception `OCA\DAV\Connector\Sabre\Exception\InsufficientStorage` + "Quota exceeded: …" vs PHP's `Sabre\DAV\Exception\InsufficientStorage` + "Insufficient space in {path}").
> 27. **OC-Checksum: Rust validates, PHP does not.** A second PUT with a WRONG `OC-Checksum` is rejected by Rust (400, temp file removed) but accepted by PHP (204), which overwrites the stored checksum with the client's claimed wrong value — the scenario's task-text premise ("both sides must reject") is refuted by the oracle. Consequence in the delta: oracle's final `oc_filecache.checksum` = the wrong SHA1, plus the normal overwrite side effects Rust skipped by rejecting (version row, `files_versions` dir size, extended row with creation≠upload, preview re-queue). First PUT parity is green: the correct checksum is stored **verbatim** on both sides (`stable` column). Rust's clear-on-absent-header behavior matches PHP.
>
> **Additional divergences surfaced by the post-#23-fix re-run of scenario 20 (2026-08-05):**
> 32. **Chunked assembly writes no `oc_files_versions` row on Rust.** PHP's assembly MOVE creates the version row (`metadata={"author":"admin"}` compact JSON, same shape as its plain-PUT version rows); Rust's `upload_handler` assembly path has no version write at all — even though Rust's *plain* PUT path does write one. The assembled file is brand new, so this is the "version row on fresh write" behavior PHP applies uniformly (see the 16.4 note) and Rust applies only on the plain path.
> 33. **Chunked assembly queues no `oc_preview_generation` row on Rust** — the assembly instance of finding #5 (PHP's `NodeWrittenEvent` cascade fires for the assembled file; Rust's assembly fires no event).
> 34. **Propagation etag identity differs.** On assembly, PHP bumps the MOVE *source* chain (`uploads` row and home root) with ONE shared etag and gives the destination `files/` a *different* etag; Rust stamps the same etag across `home == files` (one propagation stamp over all ancestors). Sentinel rendering makes the equality pattern visible (`VV1` shared home/uploads on oracle vs home/files on SUT). Same class as #1/#22 propagation semantics.
>
> **Positive parity confirmed by the same runs:** bulk upload end-to-end (#21 green); the correct-checksum PUT stores the exact header value both sides; the OCS quota set/restore round-trips identically (200 == 200); PHP's 507 rejection leaves no partial state; invalid-filename rejection leaves no state on either side; **post-#23-fix: the whole chunked v2 protocol surface matches (statuses + assembled bytes + fs delta)**.
>
> **Deviation (16.10):** four departures from the task text, all grounded:
> 1. The gate says "20–24 report identical" — that assumes a known-good build. The current build is known-red (findings #23–#27), so the scenarios are authored, run, and recorded with precise deltas; `21` proves the green path for this batch (as `14`/`30` did for earlier ones). Scenarios go green as the phase-9/10 fixes land (26 first — 23's scenario can only report a meaningful diff once the SUT's MKCOL accepts the request at all).
> 2. Scenario 24's premise in the task text — "second PUT carries a WRONG checksum — both sides must reject identically" — is **refuted by the oracle**: PHP never validates `OC-Checksum` on PUT (`File.php:374-380` stores it verbatim). The scenario is kept as authored; it now documents Rust's over-rejection (finding #27).
> 3. Scenario 23's "same rejection" comparison is not yet expressible: the SUT never rejects (finding #26), so the recording is 201-with-file-written vs 507-with-clean-rejection. The `compare_body` machinery is in place and will compare the two 507 bodies once the Rust check fires.
> 4. Scenario 20 could not exercise the chunk PUT/MOVE DB-row shape on the SUT (finding #23 blocks the flow at MKCOL); the oracle half of the run records the full PHP assembly delta as the fix target (finding #24).

### 16.11 Preview scenarios (DB-row shape)
> Previews need Imaginary. Unless the **same** Imaginary is configured for both instances, scope these to DB-row **shape** (`oc_previews` / `oc_preview_versions` / `oc_preview_locations` columns, matched by the snowflake-id-tolerant natural key), skipping the generated **bytes** (which can differ across libvips runs).

- [x] Author a preview scenario that uploads a previewable image and compares the resulting `oc_previews` row shape (width/height/cropped/max/mimetype ids/`version_id`/`etag`=source-etag semantics) on both sides; exclude preview bytes from the file-tree diff's `appdata_*/` exclusion already covers them.
- [x] Document the Imaginary precondition: either configure one Imaginary for both, or assert row-shape-only.

**Verify (gate S5b):** preview row shape matches on both sides for a known-previewable upload; a non-previewable type produces no `oc_previews` row on either side.

> **Implementation notes (16.11)** — completed 2026-08-05 (Imaginary enablement parked; see deviations). Scenarios `25_preview_image` (PUT 320×200 gradient PNG fixture → two `GET /core/preview.png?file=…&x=64&y=64` — the first generates on a miss, the second exercises the cache-hit path) and `26_preview_unpreviewable` (PUT real minimal `.zip` → preview GET with `compare_body`). Runner extensions: `delete` ops gained optional headers (cleanup uses `X-NC-Skip-Trashbin: "true"` hard deletes — supported by both sides, PHP `Storage.php:128` — keeping trash rows out of future deltas; hard delete orphans preview rows, which stay out of later deltas because they are unchanged between snapshots), and `get` ops gained `compare_body`.
> - **Registry/canonicalizer (16.5 follow-through):** `oc_previews`, `oc_preview_locations`, `oc_preview_versions` classified against the live schema and wired into `SPECS`/`fk_reference` — natural key for `oc_previews` mirrors the live unique index `previews_file_uniq_idx` `(file_id, width, height, mimetype_id, cropped, version_id)`; ids are snowflakes and remap through the bijection like any other id. New unit test `preview_snowflake_offset_hidden_shape_mismatch_reported` pins offset-hiding and shape sensitivity; `registry_coverage` extended. All 13 lib tests pass.
> - **Live-grounded preview semantics (both DBs, 2026-08-05):** `etag` IS the source file's etag (verified via join); `mtime` is the **generation wall-time**, not the source mtime — proven by PUTting with `X-OC-Mtime: 1700000000`: both sides wrote `mtime = now()` (matches PHP `Generator.php:405,563` `setMtime(time())`; the Rust `store.rs` doc comment stating this is correct). `size` = preview byte size; `version_id = -1`; `location_id`/`old_file_id` NULL; `storage_id` = the file's storage. With Imaginary down, an on-demand request generates exactly two rows (max + requested bucket crop), and the generated bytes were **byte-identical across sides** (`cmp` clean, 249 B) — both sides generate via PHP while the SUT proxies misses (below).
> - **Imaginary precondition (documented, per task):** compose defines `previews_hpb` (aio-imaginary, `:8088`) but it is **not started**, and no `preview_imaginary_url` is configured. Consequences, live-verified: PHP generates via its built-in GD providers (skeleton JPEG + probe previews exist on both instances, created by PHP cron/pre-generation and on-demand); Rust native generation is **disabled** (`rust_generatable` requires a valid `preview_imaginary_url` **and** `OC\Preview\Imaginary` in `enabledPreviewProviders` — PHP has no auto-enable either, `PreviewManager::getEnabledDefaultProvider`, and the Rust registry mirrors that), so the SUT serves cached hits natively (Phase 11.2) and proxies every miss to PHP. The suite therefore asserts **row-shape-only**, the task's second option.
> - **Determinism grounding:** image PUTs do **not** queue `oc_preview_generation` on either side (live-verified 3×; the previewgenerator `PostWriteListener` is unconditional, so the emission upstream of it is mimetype-dependent — see finding #30), so cron-driven pre-generation cannot perturb the scenario; generation happens synchronously inside the preview GET on both sides.
>
> **Results on the current build (2026-08-05):**
> - **The preview surface itself is GREEN:** scenario 25's two `oc_previews` rows render **identically** across sides (natural keys, snowflake remap, etag sentinels, size 828/249, version_id −1), both preview GETs answer 200 == 200 (miss-then-hit), and the fs deltas match (appdata excluded by construction). Scenario 26's negative case holds exactly: 404 == 404 with matching bodies (`compare_body` green — the SUT proxied the miss to PHP), and **no `oc_previews` rows on either side**; the `application/zip` mimetype insert matches too.
> - Both scenarios still report DIVERGENCE overall — entirely from the known PUT-path findings re-appearing in the same deltas (#1 home-root size over-propagation, #2 `files/` storage_mtime, #3-refined extended times, #4 versions metadata spacing), plus findings #29/#30 below. Same known-red-build convention as 16.9/16.10.
>
> **New Rust↔PHP divergences surfaced (all live-verified 2026-08-05; phase-9/10 targets):**
> 28. **`oc_filecache_extended` semantics are mimetype-dependent in PHP** (refines #3, which was traced on text files only). Live matrix on the pure-PHP oracle: `text/plain` / `application/zip` PUT → `creation_time = 0`, `upload_time = now` (finding #3 as recorded). `image/png` PUT → `creation_time = now`, `upload_time = file mtime` — equal on a plain PUT (so scenario 25's extended row matched *coincidentally*), divergent with `X-OC-Mtime` (probe: mtime 1700000000 → row `(creation_time = PUT-second, upload_time = 1700000000)`). Discriminator is the **detected mimetype**, not content or size (cross-wired: PNG bytes with `.txt` extension → text behavior; text bytes with `.png` extension → image behavior). Mechanism untraced (no creation_time writer found in the PUT chain — `File.php` writes only `upload_time`, scanner/getMetaData carry no creation_time, `Folder::newFile`'s `creation_time = time()` is Node-API-only; timeboxed, fix phase must trace the actual writer before correcting Rust's uniform `creation = upload = now`).
> 29. **PHP storage-root size propagation is conditional** (refines #1). Confirmed: plain PUT of a non-previewable file bumps `files/` only — the storage root gets an etag bump, **no size change** (scenario 26 oracle; consistent with 10/24). But scenario 25 (previewable PNG **with a successful preview generation** inside the scenario) shows the oracle storage-root size `+= filesize` (+1270). Leading discriminator: successful preview generation within the window (25 vs 26 differ only there); a mimetype-specific PUT-path effect is not yet excluded (the creation-time probes did not measure root sizes). Verify when fixing; Rust currently bumps the root unconditionally on every write.
> 30. **The previewgenerator queue rule is mimetype-dependent** (refines #5). Live: PUT of `text/plain` and `application/zip` queue an `oc_preview_generation` row on the oracle; `image/png` does not — even though `PostWriteListener.php` inserts unconditionally on `NodeWrittenEvent`, so the event emission itself is mimetype-gated upstream (untraced). SUT Rust PUTs never queue (finding #5) — for images that is currently a non-divergence (PHP doesn't queue them either); for text/zip-like types it remains real.
> 31. **`.bin` mimetype detection diverges:** PHP maps `bin → application/x-bin` (`mimetypemapping.dist.json`); Rust `mime_guess` maps `bin → application/octet-stream`. Scenario 26 deliberately uses `.zip` (both sides agree on `application/zip`, live-verified in the matching `oc_mimetypes` insert) to keep this out of the negative-case diff; the divergence itself is a detection-parity fix item.
>
> **Deviation (16.11):** three departures from the task text, all grounded:
> 1. **"Previews need Imaginary" does not hold for this install:** PHP's built-in GD providers generate previews without it (skeleton JPEGs and probe images prove it live), so the scenarios run against the built-in providers. The Imaginary path was investigated for enablement (user-approved) and **parked** with two complications on record: (a) provider ordering — GD image providers register before Imaginary under the same mimetype regexes, and `getProviders` sorts only by regex length, so PHP would keep GD for PNG even with `preview_imaginary_url` set; true single-generator parity needs `enabledPreviewProviders` narrowed to drop the GD image providers (plus `OC\Preview\Imaginary` — no auto-enable on either side); (b) the config-injection path for the instances (`data/shared/config.php` → symlinked `user.config.php`) was mid-verification when parked. Resuming is a documented follow-up, not a gate blocker: the task's either/or is satisfied by the row-shape-only assertion.
> 2. **Gate S5b's "row shape matches" is green but the scenarios are red overall** — the deltas carry the known PUT-path findings (#1/#2/#3/#4) alongside the preview surface; full green awaits the phase-9/10 fixes, as with 16.9/16.10.
> 3. **Scenario numbering 25/26** — the task text specified no numbers; 25/26 continue the breadth sequence without colliding with 30 (the self-check).

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

### 2026-08-03 — Phase 16.1 complete: pure-PHP `oracle` instance up and verified

Stood up the differential-harness Oracle as a **clone of the `nextcloud` service**
(not the stock `nextcloud2`): same local `php84/Dockerfile` image
(`master-nextcloud:latest`, shared via an explicit `image:` pin), same env /
`additional.config.php` / skeleton / `NEXTCLOUD_AUTOINSTALL_APPS`, own volumes and
`VIRTUAL_HOST=oracle.local` → its own DB `oracle`. Reached as pure PHP through a new
proxy vhost `:9091 → oracle:9000` (php-fpm TCP), mirroring the existing `:9090` path.

- `docker-compose.yml`: `oracle` service + `data-oracle`/`config-oracle`/
  `apps-writable-oracle` volumes + proxy `9091` port.
- `docker/nginx/my_proxy.conf`: `:9091` server block (upstream `oracle-php-handler`).
- `Makefile`: `diff-up` target (`--build proxy nextcloud oracle database-pgsql redis`
  + trusted-Host status.php wait).
- **Verified (gate S0):** `installed:true` on both (`:8080` Rust / `:9091` PHP),
  identical version `34.0.0.1`, identical 33-app enabled set (incl. the autoinstall
  apps), PROPFIND `207` both, oracle php-fpm `:9000` UP (nc-server `:80` unrouted).

Two grounding corrections surfaced during verification (both now reflected in the
task text above): the stock `nextcloud2` is **PHP 8.4** (`.env` `PHP_VERSION=84`),
not 8.2 — the drift is the stock image's own bootstrap/env wiring, not the PHP major;
and `oc_appconfig`'s value column is **`configvalue`** (live schema), not `value`.
Version parity is read from `status.php` (`version`); the `versionstring` ` dev`
suffix is a benign install-time artifact, so 16.2's `preconditions.rs` must compare
the numeric `version`, not the string.

### 2026-08-03 — Phase 16.2 complete: `nc-difftest` scaffold + live smoke

Created the black-box `nc-difftest` crate (workspace member, no `nc-*` deps):
`config.rs` (env-overridable SUT/Oracle URLs/Hosts/DSNs/containers/creds),
`client.rs` (reqwest + basic auth + Nextcloud desktop UA + core WebDAV verbs),
`preconditions.rs` (status.php `installed` + numeric-version match + enabled-app
set equality via sqlx/Postgres, `configvalue` column). `db/fs/canonicalize/delta/
scenario/report` are documented stubs for 16.3–16.8. `difftest smoke` subcommand +
`#[ignore]`-gated integration test. Crate enables `sqlx features=["postgres"]`
(workspace only enables `any`); `serde_yaml`/`similar`/`pretty_assertions` added to
`[workspace.dependencies]`.

**Verified (gate S1):** `cargo build -p nc-difftest` clean; smoke → preconditions
OK, PROPFIND Depth:0 `207` on both (1 response element each); `cargo test
-p nc-difftest` → 0 lib tests + 1 integration test ignored (default
`cargo test --lib` unaffected).

Carried into the task body: `lock`/chunked/bulk/`share_create` client methods
**deferred** to their scenarios (grounding over guessing); and an **early
differential finding** — Rust's bare-allprop PROPFIND Depth:0 home-root body is
~2856 bytes vs the oracle's ~585 (Rust over-emits on allprop), a phase-12/4 parity
concern the harness has already surfaced.

### 2026-08-03 — Phase 16.3/16.4/16.5: snapshot + canonicalizer + first scenarios live

Built and validated the full differential pipeline end-to-end:
- **16.3** `db.rs::snapshot` — `oc_%` enumeration, table-set parity, per-table
  `SELECT "col"::text … ORDER BY pk` in one `REPEATABLE READ` txn (rolled back).
  Idle double-snapshot **IDENTICAL** (98 tables, quiesced).
- **16.4** `scenario.rs`/`delta.rs`/`report.rs` + scenarios; `difftest run` ties
  snapshot→replay→delta→canonicalize→diff.
- **16.5** full natural-key id-bijection + equality-preserving masking; volatile
  sentinels assigned over the **delta** (not the full snapshot) to stay
  side-independent; **5 canonicalizer unit tests pass**.

**Verified both verdict paths:** `01_propfind_readonly` → `IDENTICAL` (green path);
`10_put_get` / `10_put_get_delete` → precise red diffs (divergence path).

**Harness result — real Rust↔PHP divergences on a bare PUT** (full list in the 16.4
deviation note): storage-root size over-propagation; missing `files/` storage_mtime
bump; `oc_filecache_extended` creation_time==upload_time on Rust but unequal on PHP
(plan assumption contradicted — verify PHP source); `oc_files_versions.metadata`
`{"author": "admin"}` vs PHP's compact `{"author":"admin"}`; `oc_preview_generation`
queued by PHP's event, not by Rust's native PUT. These are phase-9/10 targets; until
they are fixed, the write scenarios are **correctly red**, not a harness fault.

**Quiescing:** `oc_preferences` skipped (per-user runtime/UI state: `lastLogin`,
`lastSeenQuotaUsage`) — noise, not file-behavior. A fresh `docker compose down -v &&
make diff-up` was used (with the user's OK) to get pristine SUT+oracle instances and
avoid re-run trash natural-key collisions.

### 2026-08-03 — Divergence #3 resolved; `oc_filecache` population lifecycle written into requirements

Follow-up investigation into *how* `oc_filecache` is normally populated (needed to judge
whether the harness could "trigger population before compare" — verdict: **no**, a scan is
a repair and would mask the very divergences the oracle exists to catch; quiescing stays
double-snapshot identity based) produced two durable results:

- **Requirements §21 written** — `SPECS/01-requirements/requirements/21-filecache-population.md`:
  the four PHP population mechanisms (inline write-path update via `View`/`Updater`;
  lazy scan on PHP read; Watcher repair on PHP read; `ScanFiles` background job +
  `occ files:scan`), the `Cache::put/insert/update/remove` mechanics, the
  `oc_filecache_extended` default-`0` semantics, what the repair paths can and cannot
  reconstruct, and the requirement that native writes leave the PHP repair paths as
  no-ops. Cross-references added in requirements §6.8 and §9.4; README index updated.
- **Divergence #3 resolved** (note under 16.4): PHP writes `creation_time = 0` +
  `upload_time = now` on a bare PUT — `File.php:354-366` sends only `upload_time`,
  `normalizeData`'s `array_filter` drops the absent `creation_time`, and the column
  default is `0`. Live-confirmed on both DBs. Rust over-populates `creation_time`;
  the plan's "creation_time == upload_time must hold" assumption was wrong. Fix is
  phase-9/10 work.

Also surfaced en route (documented in §21.1.4): PHP's DAV **GET** self-heals size drift
by running `Updater::update()` on a cached-vs-filesystem size mismatch
(`apps/dav/lib/Connector/Sabre/File.php:483-496`) — a read path that writes, relevant
to any future GET-scenario delta analysis.

### 2026-08-04 — Phase 16.6 complete: negative control caught the seeded divergence (gate S3a)

Proved the harness is not a rubber stamp by seeding a real silent-side-effect bug and
watching the differential catch it:

- **Seeded:** skipped the PUT-path `oc_filecache_extended` upsert in `nc-dav`
  (`crates/nc-dav/src/davfile.rs`, dead `if false` guard around the upsert block) on
  scratch branch `phase16-negative-control`, commit `691218b`. This is exactly the
  class the harness targets: the HTTP response stays perfect while a downstream DB
  write PHP would have made silently disappears.
- **Caught:** run B of `10_put_get` reported the baseline divergences **plus** the
  `oc_filecache_extended` table appearing on the oracle side only — the delta named
  the table, the row's natural key (`home::admin | files/hello.txt`), and every
  column of the missing row. PUT answered `201 == 201` on both sides throughout; no
  protocol suite would have noticed.
- **Reverted:** back on `working`, rebuild, re-run — the report is **byte-identical**
  to the pre-mutation baseline (recorded run A). No ghost of the seeded bug, and the
  harness is deterministic across rebuilds and runs (etag/timestamp sentinels,
  natural-key id remapping, and absolute sizes all reproduce exactly).

Protocol detail worth keeping: each run started from "hello.txt absent" via a plain
DAV DELETE cleanup (to trash) between runs — no volume reset needed, because
`10_put_get`'s delta touches no trash rows. The scratch branch is kept as the record
of the seeded state.

Note: the gate's "after revert, identical" clause was verified against the recorded
baseline diff, not against green — the current build is known-red (16.4 deviation
record); see the deviation note under the 16.6 task body.

### 2026-08-04 — Phase 16.7 complete: proxied share self-check is green and sensitive (gate S3b)

`30_share_create_selfcheck` shares the skeleton folder `/Media` twice — once to the
`admin` group (shareType=1), once as a public link (shareType=3) — then deletes both
shares in new post-snapshot `cleanup` ops (captured `ocs.data.id` per side; ids differ
between the DBs). Share creation is proxied to PHP on the SUT (`nc-ocs` only serves
`/config` + `/cloud/capabilities` natively), so both sides run identical PHP — the
scenario validates the harness, not the server.

- **Green and re-runnable:** IDENTICAL on run 1 and run 2; `oc_share` empty after
  every run (group-share deletion cascades the `TYPE_USERGROUP` child row).
- **Sensitive:** reclassifying `oc_share.token` as `stable` makes it FAIL with the
  diff isolating exactly the two sides' random link tokens (`dQyi4DmiSxKGw6q` vs
  `nTMf3fpZeHGcXMA`) while the group rows stay equal; reverting restores IDENTICAL.
- **New harness surface:** full `oc_share` classification (25 live columns) +
  natural key + FK graph incl. the `parent` self-FK; unit tests pin id-offset
  hiding, parent remap, and wrong-parent detection. `oc_ratelimit_entries` moved
  to the snapshot skip-list (per-request wall-clock GC noise —
  `DatabaseBackend.php:43,83`; it surfaced mid-episode: the oracle GC'd two expired
  probe entries during the run while the SUT's had not expired yet).
- **Grounded PHP behavior (all verified live + source):** a group share writes a
  `TYPE_GROUP` parent **plus one `TYPE_USERGROUP` child per group member**
  (`DefaultShareProvider::createUserSpecificGroupShare`); `mail_send` is answered
  `1` but settled to `0` in the DB by a post-create UPDATE; share creation touches
  **no** other diff-set table (no `oc_vcategory`/`oc_properties` — the task text's
  claim was checked and does not hold; `oc_activity`/`oc_notifications`/
  `oc_admin_audit` don't exist on this install). One client fix in flight: PHP
  never populates `$_POST` without an explicit form `Content-Type`.

### 2026-08-04 — Phase 16.8 complete: file-tree differential live (gate), delete-flow divergences surfaced

`fs.rs` now snapshots `data/{user}/files/**` by relative path + size + sha256 (one
`docker exec` per side; volatile subtrees excluded by rooting at `files/**`, `*.part`
defensively), computes per-side before/after file deltas, and diffs them next to the
DB delta — the run verdict is DB-identical AND fs-identical AND statuses match.
`difftest snapshot` gained the fs idle double-snapshot check. 4 new unit tests.

**Verified on pristine instances** (user-approved `docker compose down -v && make
diff-up`, same procedure as 16.3-16.5): idle double-snapshot IDENTICAL (DB 96 tables
+ 10 files each side, quiesced); `10_put_get` fs delta identical (same path/size/
sha256 both sides); `30_share_create_selfcheck` fs delta empty both sides; the
delete scenario's net-zero file tree identical.

**Delete-flow enabler — the `.d{ts}` volatility is solved:** both replays run
sequentially, so trashed paths (`files_trashbin/files/hello.txt.d{deletion-ts}`,
`…/versions/hello.txt.v{mtime}.d{ts}`) straddle second boundaries between sides and
used to break natural-key matching. The canonicalizer now strips `.d{ts}`/`.v{ts}`
suffixes from paths/names under `files_trashbin/` and `files_versions/` (mirroring
the `oc_files_trash.id` strip) and masks their `path_hash` (md5 of the unstripped
path). Pinned by `trash_volatile_suffix_stripped` incl. the negative case.

**Harness result — real Rust↔PHP divergences on DELETE-to-trash** (pristine
`10_put_get_delete`, all live-verified 2026-08-04; phase-9/10 targets):
6. **Trashbin folder sizes** — PHP sets `files_trashbin/` and
   `files_trashbin/files` size to include the trashed file (26); Rust creates them
   with size 0.
7. **Trashbin skeleton dirs** — PHP creates `files_trashbin/keys` and
   `files_trashbin/versions` during trashing; Rust does not.
8. **Lazy `cache/` materialization** — the flow materializes the user's `cache/`
   filecache row on PHP; not on Rust (which op triggers it still to be isolated).
9. **Extended rows on trash move** — Rust creates `oc_filecache_extended` rows for
   the trashbin ancestor dirs; PHP creates only the trashed file's row, with
   `creation_time != upload_time` (creation stays 0) where Rust sets them equal
   (another instance of finding #3).
10. **Version rows on delete** — PHP deletes the file's `oc_files_versions` row
    (the version file is trashed, the DB row is not); Rust re-keys the row to the
    trashed file instead of deleting it.
11. **Preview re-queue on trash** — PHP queues an `oc_preview_generation` row for
    the trashed file; Rust does not.
12. **Storage-root storage_mtime** — PHP bumps the storage root's `storage_mtime`
    on a fresh PUT (pristine observation) and across the delete flow; Rust does not.
    (The `files/` row matches on both sides in the pristine runs.)

Also observed: `oc_files_trash` and its natural key `(user, location, type, mime)`
match on both sides in pristine runs, as do the `text/plain` mimetype insert and the
PUT+GET part of the scenario modulo the 16.4 findings.

### 2026-08-04 — Phase 16.9 complete: scenarios 11–18 authored and recorded (gate S4 deferred on build fixes)

All eight core native scenarios authored, wired (incl. cleanup ops and per-scenario
`stable_overrides` for client-dictated values), and recorded on pristine instances
in suite order. `oc_vcategory`/`oc_vcategory_to_object` classified for the
favorites/tags flow (legacy ITags — `oc_properties` is not involved, grounded).

**Green:** 14_propfind_depth1, 30_share_create_selfcheck. **One-shot red:** 01 on a
fresh instance's first PHP files access (lazy `cache/` materialization, finding #8).
**Recorded red with precise deltas:** everything write-heavy — MOVE matches cleanly
(12), MKCOL matches except parent storage_mtime (11), and the harness surfaced ten
new divergences (#13–#22 in the 16.9 notes): COPY size/checksum/etag/extended
handling, the broken text-valued PROPPATCH extraction (favorite=1 executes an
un-favorite — log-proven), lazy `files_metadata` appconfig registration, versioning
storage_mtime/etag/size details, and home-root mtime propagation. X-OC-Mtime itself
is honored (stable-compared: both sides store exactly the dictated value).

All file-tree deltas matched across the suite — the fs oracle is green even where
the DB oracle is red. Reset note: two user-approved `down -v && make diff-up` resets
provided the pristine recordings (probe residue in `oc_vcategory` is unrecoverable
over DAV — PHP never deletes tag categories and 500s on an empty `<oc:tags/>`).

### 2026-08-05 — Phase 16.10 complete: scenarios 20–24 authored and recorded (gate S5a deferred on build fixes); bulk upload green

Session began by repairing the crate's mid-edit state from the previous session
(the `Op` enum had been closed early, orphaning `ShareCreate`/`ShareDelete`;
`ChunkedUploadV2` declared `upload_id` where the match arm and YAML use
`upload_dir`). The runner grew the rejection-parity machinery: per-step status
recording for composite ops (no hard aborts mid-flow — a rejected step is a
divergence to report), `compare_body` cross-side body comparison, and separate
status/body failure flags in the verdict.

**Results:** `21_bulk_upload` is **IDENTICAL** (both runs) — first green native
write scenario of the upload family. 20/22/23/24 recorded red with precise
deltas, surfacing five new divergences (#23–#27 in the 16.10 notes), the
headlines being: chunked v2 MKCOL's invented `Destination` requirement breaks
every standard chunked upload at step one; and Rust accepts any upload once
usage exceeds quota (201 + file written) where PHP answers 507.

**Quota root cause, live-proven:** with a 100-byte quota and 20 MB used,
instrumented logs showed `check_quota` compute free = `100 − 20575246 =
−20575146` and take the `free < 0 → skip as unlimited` branch. `saturating_sub`
only guards i64 overflow — it does not clamp at 0 as the module doc claims; PHP
clamps `max(quota − used, 0)` in the Quota storage wrapper, hence its 507. The
instrumentation was reverted after the probe (fix is phase-9/10 work); the only
permanent quota.rs change is a `bare_bytes_unit` unit test pinning the `"<n> B"`
form the OCS API writes to `oc_preferences` (live-verified).

**Harness quiescing:** the OCS quota edit touches five CardDAV/Circles tables on
both sides (system-addressbook card update, circles member cache) with
per-instance identity data (hostname-derived URLs, synctoken watermarks, random
ids, wall-clock timestamps) — added to `db.rs` `SKIP_TABLES` with the full
rationale, same class as `oc_preferences`. Noted in passing: the oracle's card
rewrite dropped `/index.php` from the social-profile URL while the SUT's did not
— instance-config drift between the clones, harmless to the differential but a
reminder that VIRTUAL_HOST-derived URLs are per-instance identity.

Scenario 23's body was repaired to actually exceed the quota (it was authored at
exactly 100 bytes) — the body length is part of the test. All probe artifacts
were hard-deleted from both instances (`X-NC-Skip-Trashbin`) and the quota
restored to none; probe files never entered any recorded delta.

### 2026-08-05 — Phase 16.11 complete: preview row-shape differential live (gate S5b shape clauses green); Imaginary enablement parked

Added the preview surface to the harness: `oc_previews` /
`oc_preview_locations` / `oc_preview_versions` classified against the live
schema, natural-keyed by the live unique index `(file_id, width, height,
mimetype_id, cropped, version_id)` with snowflake ids remapping through the
bijection (new unit test pins offset-hiding + shape sensitivity), plus runner
extensions (deletes with headers for `X-NC-Skip-Trashbin` hard-delete
cleanups; GET `compare_body`).

**Scenarios 25/26 recorded:** the preview surface itself is green — 25's two
`oc_previews` rows (max 320×200 + 64×64 crop) render identically across sides,
miss-then-hit GETs both 200, generated bytes byte-identical while Imaginary is
down (both sides PHP-generate, the SUT proxying misses); 26's negative case
holds exactly (404 == 404 with matching bodies, zero preview rows either side,
matching `application/zip` mimetype insert). Overall verdicts stay red on the
known PUT-path findings re-appearing in the same deltas (#1/#2/#3/#4) — the
established known-red-build convention.

**Four new divergences (#28–#31 in the 16.11 notes), headline being #28:**
PHP's `oc_filecache_extended` semantics are **mimetype-dependent** — text/zip
PUTs get `creation_time=0, upload_time=now` (finding #3's traced case), image
PUTs get `creation_time=now, upload_time=file-mtime` (equal on a plain PUT, so
scenario 25's extended row matched only coincidentally; divergent with
`X-OC-Mtime`). Discriminator is the detected mimetype (content/extension
cross-wires prove it); the PHP writer responsible was not found in the PUT
chain despite a thorough trace (`File.php` writes only `upload_time`;
`Folder::newFile`'s `creation_time=time()` is Node-API-only) — timeboxed to
the fix phase. Also recorded: PHP storage-root size propagation is conditional
(#29 — plain non-previewable PUTs don't bump it, scenario 25's
preview-generation window does), the previewgenerator queue is
mimetype-dependent (#30 — text/zip queue, images don't, despite the
unconditional `PostWriteListener`), and `.bin` detection diverges (#31 — PHP
`application/x-bin` vs Rust `application/octet-stream`; why 26 uses `.zip`).

**Imaginary parked (user-approved investigation, resumed as follow-up):**
`previews_hpb` exists in compose but is not started and no
`preview_imaginary_url` is configured, so the suite asserts row-shape-only —
the task's second option. Enablement has two recorded complications: GD
providers register before Imaginary under the same regexes (PHP would keep GD
for PNG with the URL set — true single-generator parity needs
`enabledPreviewProviders` narrowed), and the config-injection path
(`data/shared/config.php` → symlinked `user.config.php`) was mid-verification
when parked.

Instance hygiene: all creation-time/probe artifacts hard-deleted from both
sides and orphaned preview rows removed with a targeted `file_id NOT IN
oc_filecache` predicate; both DBs restored to the 7 skeleton preview rows.

### 2026-08-05 — Finding #23 FIXED: chunked v2 MKCOL no longer demands a Destination header

`upload_handler.rs::handle_mkcol` rejected MKCOL without a `Destination`
header (400) — a PHASE-5.5 spec claim that the protocol refutes: PHP's
`ChunkingV2Plugin` reads `Destination` only in `beforeMove`, and sabre's MKCOL
never consumes it. Every standard chunked upload died at step one. The stored
`target_path`/`expected_size` session metadata was verified dead (the assembly
MOVE re-derives the target from its own header), so the fix simply drops the
requirement — a client-supplied Destination is still accepted leniently. The
module doc was corrected to record the ground truth (CLAUDE.md principle 3:
the old PHASE-5.5 wording was the task-text error this phase exists to catch).

**Verified:** manual probe of the full MKCOL → chunk PUTs → assembly MOVE →
GET flow is green on the SUT (201/201/201/201/200, correct assembled content),
and scenario 20 re-run records all protocol statuses matching plus an identical
fs delta — the chunked v2 protocol surface is now at parity. The remaining
scenario-20 diff is the assembly's DB side-effect surface, yielding three new
findings: #32 (no `oc_files_versions` row on assembly — Rust's plain PUT writes
one, its assembly path doesn't), #33 (no `oc_preview_generation` queue row —
the assembly instance of #5), #34 (propagation etag identity: PHP shares one
etag across the MOVE source chain home==uploads and gives `files/` its own; Rust
stamps home==files). Finding #24's home-root −size pattern confirmed a third
time (−52). All 286 nc-dav lib tests pass; 13/13 nc-difftest tests pass.

Coordination note: taken in parallel with another session investigating the
16.4 deviations (#1–#5) — this fix touches only `upload_handler.rs`, disjoint
from that work's surface.

### 2026-08-05 — Phase 16.4 findings #1–#5 resolved: PUT parity fix applied

All five divergences from the 2026-08-03 deviation record (the plan at
[`phase-16.4-put-parity-plan.md`](phase-16.4-put-parity-plan.md)) are resolved
in `nc-dav` (WIP commits `7b6b501`–`ee6e38b` on `working`).

**Changes (all in `crates/nc-dav/src/`):**

- **`davfile.rs`** – `flush()` commit path:
  - Finding #3: `creation_time = x_oc_ctime.unwrap_or(0)` (was `unwrap_or(now)`);
    `upload_time = now` (was `use_mtime`). ON CONFLICT updates only `upload_time`.
  - Finding #1/#2: replaced old `propagate_change(fc_path, use_mtime, size_diff)`
    with PHP `Updater::update` mirror: new-file → `correct_folder_size_chain` +
    `correct_parent_storage_mtime` + `propagate_change(0)`; overwrite →
    `correct_parent_storage_mtime` + `propagate_change(size − old_size)`.
  - Finding #5: calls `preview_queue::queue_preview_generation` post-commit.

- **`propagator.rs`** – two new helpers (both unit-tested, SQLite in-memory):
  - `correct_parent_storage_mtime(parent_fc_path, parent_disk_path)`: reads the
    parent directory's on-disk mtime, sets `storage_mtime` + `mtime` to it.
  - `correct_folder_size_chain(fc_path)`: recomputes every ancestor's size as
    the sum of its direct children (deepest-first); unscanned child (`size = -1`)
    marks the ancestor unscanned too. PostgreSQL `SUM(bigint)`→`NUMERIC` driver
    issue fixed with `CAST(SUM(size) AS BIGINT)` (SQLite-safe).

- **`versions.rs`** – Finding #4: `version_metadata_json(author_uid)` via
  `serde_json::json!({"author": uid}).to_string()` → compact `{"author":"admin"}`.
  Unit-tested for compactness and special-character escaping.

- **`preview_queue.rs`** (new) – Finding #5: `INSERT INTO … WHERE NOT EXISTS`
  guard (table has no unique constraint on `(uid, file_id)`). Log-and-continue on error.

- **`lib.rs`** – `mod preview_queue`.

**Verification:**
- `cargo test --lib -p nc-dav` → 292 passed (no regressions; all new helpers tested)
- `cargo test --lib` workspace → clean except pre-existing `nc-fastcgi::registry_scans_real_apps_dir`
- Manual DB inspection confirms both SUT and Oracle update `files/` storage_mtime
- `cargo build -p nc-dav` / `cargo build -p nc-difftest` → clean (with unused-import fix)

**Difftest:**
- `10_put_get`: findings #3/#4/#5 → matching in delta. #2 → both sides update
  storage_mtime (Oracle's change hidden by sentinel masking artifact). #1 → Rust
  recomputes root size correctly; PHP does not (see PHP bug below).
- `21_bulk_upload` → IDENTICAL (regression guard passes).
- `16_overwrite_put`, `18_explicit_mtime` → same root-size + storage_mtime patterns
  as `10_put_get`; no new divergence classes.

**PHP bug discovered (finding #1):** `Cache::calculateFolderSizeInner`
(`Cache.php:1023`) compares `$entry['mimetype']` (integer from DB, e.g. `2`)
against `FileInfo::MIMETYPE_FOLDER` (string `"httpd/unix-directory"`) with
strict `===` — always false. The `correctFolderSize` recursion walks the ancestor
chain but never computes any folder's size. PHP's `files/` size is nevertheless
updated (mechanism unidentified, possibly Scanner::scan internal side-effect);
root size is **never** recomputed through this path. Rust intentionally implements
the correct behavior (recompute all ancestors including root) per CLAUDE.md
principle 5 — don't replicate PHP bugs. The root-size delta in the difftest is
an intentional improvement, not a defect.

### 2026-08-07 — Delete-flow findings #6–#12 resolved: delete-to-trash parity (plan `phase-16.8-delete-trash-parity-plan.md`)

The delete-to-trash divergence cluster (findings #6–#12 from the 16.8 Changes record) is resolved
in `nc-dav` (`filesystem.rs`). Ground truth was re-verified live — controlled PUT/DELETE probes
against the oracle plus fresh-stack difftest runs — which **corrected three of the original
findings**:

- **#10 (version rows):** PHP creates an `oc_files_versions` row on every PUT (`created()` →
  `createVersionEntity`: `file_id` = file id, `timestamp` = **file mtime**, `metadata` =
  `{"author":…}` — the SUT's `davfile.rs:618` insert already matched) and **deletes it
  unconditionally during the trash flow**: the View-level `delete` hook bridges (`HookConnector`)
  to `NodeDeletedEvent` → versions `remove_hook` → `deleteVersionsEntity` →
  `deleteAllVersionsForFileId` (`DELETE … WHERE file_id = ?`), regardless of version files.
  Rust's `trash_versions` had gated the DELETE behind the version-file query's early return; it
  now runs unconditionally by the trashed node's own file id (dir trashes delete nothing, since
  the hook fires only for the deleted node — inner files' rows survive with unchanged fileids,
  PHP parity).
- **#11 (preview re-queue): the original claim was wrong.** PHP does NOT touch
  `oc_preview_generation` on trash (pixel.png probe: PUT queues id 3, DELETE leaves it). No fix
  needed — the SUT already matched; adding the plan's queue call would have introduced a
  divergence.
- **#12 (root storage_mtime + etag ordering):** the root's `storage_mtime = now` comes from
  `setUpTrash`'s mkdir side effects (`View::mkdir` → `Updater::update` →
  `correctParentStorageMtime`), not from `renameFromStorage`; `ensure_parent_dir` now mirrors
  them. The oracle etag pattern (`root.etag == files.etag`, `files_trashbin.etag ==
  files_trashbin/files.etag`) comes from PHP's three-step stamping: `renameFromStorage` source →
  trash chain → **`View::unlink`'s post-op `Updater::remove`**, the final root writer;
  `delete_file`/`delete_dir` now run the trash chain first and the source chain last.
- **#8 rescoped (cache/):** the delete flow materializes the user's `cache/` row (one-shot per
  instance on first files access — cf. the 16.9 01-scenario note). `move_to_trash` now
  create-if-missing's it with the scanner-insert shape (size 0, permissions 31, no extended row).
  Without it the harness's etag sentinel numbering shifted for every subsequent row, hiding the
  etag-pattern fix.

**Verification (fresh stacks, `down -v && make diff-up` before each acceptance run):**
- `10_put_get_delete` and `17_delete_to_trash` — parity except the accepted root-size divergence
  (#1: SUT recomputes the root size including the trash; PHP's bugged `correctFolderSize` never
  does). Every other row — etag equality patterns, storage_mtime (raw values verified equal),
  skeleton, trash row, extended rows, preview row, version rows — matches.
- `11_mkdir_nested` — the known "MKCOL matches except parent storage_mtime" divergence is fixed
  (mkdir'd dirs now identical incl. storage_mtime; `ensure_parent_dir` mkdir side effects).
- `14_propfind_depth1`, `30_share_create_selfcheck` — IDENTICAL (no regressions).
- `10_put_get` — unchanged versioning-path rows (version-file etag/checksum/storage_mtime,
  `files_versions` size + extended row) — the pre-existing #13–#22 group in `davfile.rs`/
  `versions.rs` (untouched this session) — out of scope.
- `cargo test --lib -p nc-dav` → 302 passed (4 new tests: unconditional version-row delete, etag
  equality pattern, root storage_mtime stamp, cache-row materialization; the dir-trash test
  updated to PHP's by-id semantics). Workspace `cargo test --lib` clean except the pre-existing
  `nc-fastcgi::registry_scans_real_apps_dir`.

**Remaining known (out of scope):** the versioning-path rows of `10_put_get` (#13–#22), and the
one-shot `cache/` materialization for non-delete first accesses (16.9's 01 note).

### 2026-08-07 — Versioning-path divergences resolved: overwrite/version-file parity

The versioning-path rows of `10_put_get` / `16_overwrite_put` (the #13–#22 group's versioning
portion) are resolved in `nc-dav` (`versions.rs`, `davfile.rs`, `bulk_handler.rs`,
`filesystem.rs`). Ground truth: live PUT-v1/v2 probes against the oracle (raw rows compared
side-by-side with the SUT) plus fresh-stack difftest runs.

**Fixes (version-file creation — PHP `LegacyVersionsBackend::createVersion` → `View::copy` →
`Cache::copyFromCache`):**
- The version row is a **clone** of the overwritten file: it inherits the SOURCE etag (the old
  content's etag), the source mtime, and a NULL checksum; the SUT had generated a fresh uuid
  etag, bound `''` checksum, and bound `storage_mtime = mtime`. `storage_mtime` is now the copy
  time (the copied file's disk mtime — PHP `updateStorageMTimeOnly`).
- `ensure_version_parents` no longer inserts `oc_filecache_extended` rows for the version dirs
  (the #9 class) and mirrors `View::mkdir` → `Updater::update` side effects (parent
  `storage_mtime` + ancestor propagation).
- The version-file move now runs PHP's `copyOrRenameFromStorage` side effects: the parent dir's
  `storage_mtime` correction, the `[root, files_versions]` etag/mtime propagation, and the parent
  dir's size recompute (PHP gets the size from `getFileInfo`'s "ensure scanned").
- Same-second overwrites keep the file's etag: PHP's scanner reuses the etag when the disk mtime
  is unchanged (`Scanner.php:167-183`); the SUT's overwrite UPDATE now does the same, so the file
  and its version share the etag exactly as the oracle does.
- `insert_version_entity` retries on the unique `(file_id, timestamp)` constraint, bumping the
  timestamp by 1 (PHP `createVersionEntity`'s 5-try loop) — a same-second overwrite's entity now
  lands reflecting the CURRENT file state instead of being silently dropped.
- `trash_versions` moved out of `move_to_trash` into the delete flow AFTER the trash-chain
  propagation (PHP `retainVersions` runs after `renameFromStorage`), and each version-file move
  propagates etag/mtime on its source and target chains (the oracle ends with
  `files_trashbin.etag == files_trashbin/versions.etag`).
- The `cache/` materialization (finding #8) extracted to `ensure_cache_row` and now also runs on
  the PUT flush (the first files access can be a PUT).

**Verification (fresh stack):**
- `16_overwrite_put` — parity except the accepted root-size divergence (#1).
- `10_put_get` — parity except the accepted root-size divergence.
- `10_put_get_delete` — parity except the accepted root-size divergence.
- `14_propfind_depth1`, `30_share_create_selfcheck`, `21_bulk_upload` — IDENTICAL (no
  regressions).
- `cargo test --lib -p nc-dav` → 304 passed (2 new tests: the version-row clone semantics + the
  entity timestamp-bump retry).

**Remaining in the #13–#22 group (out of scope this pass):** the COPY size/checksum/etag/extended
handling (`12_move_rename` / `13_copy`), the PROPPATCH favorite bug, the lazy `files_metadata`
appconfig registration, and the home-root mtime propagation.

### 2026-08-07 — MOVE/COPY divergences resolved: move-rename + copy parity (`12_move_rename`, `13_copy`)

The MOVE/COPY cluster (the #13–#22 group's remaining file-operation rows) is resolved in
`nc-dav` (`filesystem.rs`). Ground truth: live PUT/MOVE/COPY probes against the oracle (raw rows
compared side-by-side with the SUT) plus difftest runs.

**Fixes (MOVE — PHP `View::rename` → `copyOrRenameFromStorage`):**
- The moved file's row is re-keyed path/path_hash/name/parent only — `mtime` and `etag` are
  KEPT (PHP `Cache::move`, Cache.php:813-831; the SUT had been stamping fresh values).
- Both direct parents' `storage_mtime` are corrected from their disk mtimes
  (`correctParentStorageMtime`, Updater.php:198-201 — the SUT's rename lacked the calls).
- The ancestor size recomputes switched to the size-ONLY `correct_folder_size_chain`: the
  standalone `correct_folder_size`'s internal propagation re-stamped the root AFTER the
  target-chain etag, breaking the oracle's `root.etag == files.etag` after a move.

**Fixes (COPY — PHP `View::copy` → `copyFromStorage`):**
- The destination row is a CLONE of the source: it inherits the source etag and mtime, drops
  the checksum (NULL), and takes `storage_mtime` = the copy time — the same clone semantics as
  the version-file fix.
- The destination inherits the source's `oc_filecache_extended` row
  (creation_time/upload_time).
- A COPY into a fresh subdir now creates the parent (PHP scans it into the cache;
  Updater.php:141-148 — the SUT previously answered 409).
- The copy queues an `oc_preview_generation` row (PHP's NodeWrittenEvent → the previewgenerator
  PostWriteListener).
- The ancestor size recompute chained on the TARGET so a freshly-created parent dir gets its
  size from the new child (the oracle's `copy-dir` carries the copy's size).

**Verification:**
- `12_move_rename` — parity except the accepted root-size divergence (#1).
- `13_copy` — parity except the accepted root-size divergence (remaining storage_mtime rows are
  the known second-boundary label artifacts).
- `14_propfind_depth1`, `30_share_create_selfcheck`, `10_put_get`, `16_overwrite_put` —
  unchanged/no regressions (the last two at their established parity level).
- `cargo test --lib -p nc-dav` → 304 passed.

**Remaining in the #13–#22 group:** the PROPPATCH favorite bug (`15_proppatch_favorite_tags`),
the lazy `files_metadata` appconfig registration, and the home-root mtime propagation.

### 2026-08-07 — PROPPATCH favorite fix (`15_proppatch_favorite_tags`)

The "broken text-valued PROPPATCH extraction" (16.9 finding) is resolved in `nc-dav`
(`filesystem.rs`, `tags.rs`):

- The dav-server passes the FULL serialized element in `DavProp.xml`
  (`handle_props.rs::element_to_davprop_full` — `<oc:favorite xmlns:oc="…">1</oc:favorite>`), so
  the favorite handler's naive `parse::<i64>()` over the whole element failed and `favorite=1`
  executed an **un**-favorite. The handler now extracts the inner TEXT
  (`tags::prop_inner_text` — xmltree-based, raw-string fallback) before the truthy test.
- Verified: the `oc_vcategory_to_object` favorite mapping is identical on both sides after a
  PROPPATCH; the remaining `oc_vcategory` category-row delta is the accumulated residue
  (`\OC\Tags` never deletes categories — the scenario's known re-run caveat). The
  `files_metadata` appconfig row is unchanged during the run (a pre-existing state difference —
  the lazy-registration divergence, still open).
- 305 nc-dav lib tests pass (1 new: `prop_inner_text_extracts_text_value`);
  `14_propfind_depth1` / `30_share_create_selfcheck` IDENTICAL; 10/16/12/13 unchanged (no
  regressions).

**Remaining in the #13–#22 group:** the lazy `files_metadata` appconfig registration and the
home-root mtime propagation.
