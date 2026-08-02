# 16. Differential Integration-Test Harness: Rust core vs. PHP reference

## Context

We are about to deploy the Rust `nc-server` core in place of PHP-FPM for the hot
paths (WebDAV file ops, chunked upload, previews, OCS). The rewrite's whole
premise is *behavioral identity* with PHP (CLAUDE.md principles 1, 3, 4) — and
the recurring failure mode documented in CLAUDE.md is **silent DB side-effect
divergence**: Rust performs the visible operation correctly but skips a downstream
write PHP would have made (a cache row, an etag bump, a `oc_filecache_extended`
entry, a metadata JSON column). Protocol-compliance suites (Behat/litmus) cannot
catch these, because the HTTP response still looks right.

This harness is a **differential oracle**: run the *same* sequence of HTTP
operations against both implementations, then compare the resulting PostgreSQL
state **and** the on-disk file tree. Any divergence is a bug signal. It runs
before deploy and in CI as the equivalence gate.

### Key facts that make this cheap to build (verified)
- The stack runs **two isolated instances on one Postgres server**. The container
  bootstrap derives the DB name from `VIRTUAL_HOST`
  (`docker/bin/bootstrap.sh:239`): service `nextcloud` → DB `nextcloud`, and a
  new `oracle` service (`VIRTUAL_HOST=oracle.local`) → DB `oracle`. Both
  auto-install with `admin/admin`, the same app set, and the same skeleton —
  because `oracle` is a clone of `nextcloud` (below), **not** the stock
  `nextcloud2`.
- `nextcloud` is the **System Under Test** (Rust `nc-server` fronting PHP-FPM,
  host port `127.0.0.1:8080` via the `proxy` nginx).
- The **pure-PHP Oracle** is a NEW `oracle` compose service **cloned from
  `nextcloud`** — same local `php84/Dockerfile` image (PHP 8.4), same env
  (`WITH_REDIS`, `PRIMARY`, autoinstall apps), same `additional.config.php` and
  skeleton — but with its own named volumes and `VIRTUAL_HOST=oracle.local`, so
  the bootstrap derives a **separate DB `oracle`**. The image has no nginx and
  `bootstrap.sh:451` starts `nc-server` (Rust) on `:80` unconditionally, so the
  oracle is reached as pure PHP through a new `proxy` vhost (`:9091`) that
  fastcgi-passes to the oracle's php-fpm **TCP** pool (`oracle:9000`, the same
  `docker/configs/php-fpm-tcp.conf` the SUT mounts) — exactly the mechanism of
  the existing `:9090` PHP path, but on the oracle's own DB. `nc-server` still
  boots in the oracle container but is never routed to.
  - **Why not `nextcloud2`:** `nextcloud2` is the **stock**
    `nextcloud-dev-php${PHP_VERSION}` image — the *same* PHP major (`.env` sets
    `PHP_VERSION=84`) but a **different image** than the SUT's local
    `php84/Dockerfile`. It ships its **own** entrypoint/bootstrap/nginx, and its
    service env omits `WITH_REDIS`, `NEXTCLOUD_AUTOINSTALL`, and
    `NEXTCLOUD_AUTOINSTALL_APPS`, so it gets a different caching backend and a
    different auto-installed app set, no php-shim, no php-fpm-tcp pool, and the
    stock bootstrap's own user/config provisioning — even though its skeleton and
    `additional.config.php` mounts match. Using it as the oracle would flood the
    differential with this config drift, unrelated to Rust.
- Postgres is exposed at `127.0.0.1:8212`, `postgres/postgres`
  (container `master-database-pgsql-1`).
- Rust writes to DB itself only on the **native** paths: WebDAV file ops
  (`/remote.php/webdav/*`, `/remote.php/dav/files/{uid}/*`), chunked upload v2
  (`/remote.php/dav/uploads/*`), bulk upload (`/dav/bulk`), previews. Everything
  else is proxied to PHP and is identical-by-construction — those become
  **harness self-checks** (must always match, validating the harness itself).

No new containers or compose services are required.

## Approach

A new test-only crate **`nc-difftest`** in the `core-rs` workspace (Rust, per
decision). It is an out-of-process black-box harness: it speaks HTTP to the two
live instances and SQL to their two databases. It does **not** link the server.

For each scenario it:
1. Runs preconditions (both instances up; same `oc_version`; same enabled apps).
2. Quiesces background noise (both already run `occ background:cron`; see §5).
3. Snapshots both DBs + both file trees (**before**).
4. Replays the same HTTP op sequence against SUT and Oracle.
5. Compares normalized HTTP responses, then snapshots **after** and computes the
   **delta** on each side, canonicalizes, and diffs `delta_sut` vs `delta_oracle`.
6. Diffs the file-tree deltas (relative path + size + sha256).
7. Emits an actionable report; non-empty diff = test failure.

Comparing **deltas** (not absolute state) cancels install-time differences
(instanceid, baseline auto-id watermarks) between the two DBs.

### Layout
```
core-rs/crates/nc-difftest/          # new workspace member (test-only)
├── Cargo.toml                       # tokio, reqwest(rustls), sqlx(postgres),
│                                    # serde, serde_yaml, anyhow, tracing,
│                                    # clap, similar, pretty_assertions(dev)
├── column_registry.yaml             # per-table.column classification (§3)
├── scenarios/*.yaml                 # data-driven op sequences (§4)
├── fixtures/                        # payloads: hello.txt, 1k_random.bin, ...
├── src/
│   ├── lib.rs
│   ├── config.rs                    # base URLs, DSNs, container names (env-overridable)
│   ├── client.rs                    # NextcloudClient: reqwest + basic auth,
│   │                                #   WebDAV verbs (PROPFIND/PROPPATCH/MKCOL/
│   │                                #   MOVE/COPY/LOCK), chunked PUT v2, Nextcloud
│   │                                #   client User-Agent (avoids PHP CSRF)
│   ├── db.rs                        # Pg snapshot: enumerate oc_* tables, dump rows
│   │                                #   in one REPEATABLE READ txn (sqlx PgPool)
│   ├── fs.rs                        # file-tree snapshot via `docker exec <ctr>
│   │                                #   find ... -print0 | xargs sha256sum`
│   ├── canonicalize.rs              # id-bijection + equality-preserving mask (§3)
│   ├── delta.rs                     # snapshot → Delta (added/changed/removed rows)
│   ├── scenario.rs                  # YAML loader + Op executor + response normalizer
│   ├── preconditions.rs             # version + enabled-app parity
│   └── report.rs                    # unified-diff rendering of canonical deltas
├── src/bin/difftest.rs              # CLI: run one/all scenarios, dump report
└── tests/differential.rs            # #[tokio::test] parametrized over scenarios,
                                     #   gated behind `--ignored` / NC_DIFFTEST=1
```
Reuses existing workspace deps (`reqwest` 0.12 rustls, `sqlx` 0.8, `tokio` 1,
`serde`, `clap`). Adds `serde_yaml` + `similar` (+ `pretty_assertions` dev).

### Invocation
Add to repo-root `Makefile`:
```make
diff-up:    docker compose up -d nextcloud oracle database-pgsql redis proxy  # + wait-healthy
diff-test:  cd docker/nc-server-core/core-rs && cargo test -p nc-difftest --release -- --ignored
diff-one:   cd ... && cargo test -p nc-difftest --release -- --ignored $(S)
```
Integration tests are `#[ignore]`-gated so plain `cargo test --lib` (the project's
unit-test entrypoint per CLAUDE.md) is unaffected. A `difftest` binary gives
richer reports than the test harness.

## 3. Canonicalization & delta-diff (the core algorithm)

The diff must neutralize **incidental** nondeterminism while preserving
**structural** relationships — over-masking hides real bugs, under-masking floods
false positives. Implemented in `canonicalize.rs`, unit-tested with fixtures.

**Snapshot.** `db.rs` enumerates `oc_%` tables (assert the two sets match, modulo
an explicit skip-list), dumps each `SELECT * ORDER BY pk` inside a single
`REPEATABLE READ` transaction (consistent cross-table view; no commit).

**Column classification** (`column_registry.yaml`, keyed `table.column`):
- `stable` — compare verbatim (path, name, size, permissions, checksum, mimetype
  *name*, storage `id` string, property name/value, share perms, …).
- `id_pk` / `id_fk` — remap through a canonical bijection (below).
- `timestamp_wall` — mask absolute value, **preserve equality/ordering** across
  columns in the same row (so `creation_time==upload_time` on a fresh PUT must
  hold on both sides; a missed bump is still caught).
- `volatile_value` — random/time-based but equality is meaningful (etag): mask to
  per-snapshot sentinels that keep equal-values-equal and distinct-values-distinct
  (catches "parent got the same etag as its child").
- `volatile_independent` — per-row random, no equality expected (share `token`,
  `metadata_etag`): mask to a constant.
- `ignore` — known irrelevant (`oc_storages.last_checked`).

**id-bijection** (solves the fileid-sequence-offset problem). SUT and Oracle use
independent Postgres sequences/snowflakes, so the same logical row has different
ids that ripple through every FK. Build bidirectional `sut→canonical` /
`oracle→canonical` maps **in FK-dependency (topological) order**, matching rows by
a **stable natural key**, not by id:
- `oc_storages`: key `id` (`home::admin`); `oc_mimetypes`: key `mimetype`;
  `oc_vcategory`: key `(uid,type,category)`.
- `oc_filecache`: key `(canonical(storage), path)` — *path is the true natural
  key*. Then `oc_filecache_extended` by `canonical(fileid)`;
  `oc_vcategory_to_object` by `(canonical(objectid),canonical(categoryid),type)`;
  `oc_properties` by `(userid,propertypath,propertyname)`; `oc_files_trash` by
  `(user,id,location)`; `oc_share` by `(uid_owner,uid_initiator,item_type,
  canonical(item_source),share_with,file_target)`; `oc_preferences`/`oc_appconfig`
  by their natural keys (no id column).
- Every matched pair gets a canonical label; every `id_fk` remaps through the same
  map. **A row present on one side but not the other under a natural key is itself
  a reported divergence** (never masked). Works identically for snowflake ids
  (previews) since only uniqueness + natural-key matching matter.

**Diff set** (Rust-native, highest priority): `oc_filecache`,
`oc_filecache_extended`, `oc_files_metadata`, `oc_storages`, `oc_mimetypes`,
`oc_properties`, `oc_vcategory`, `oc_vcategory_to_object`, `oc_files_trash`,
`oc_previews`(+`_versions`/`_locations`), `oc_share`, `oc_preferences`,
`oc_appconfig`. **Self-check set** (proxied, must be identical): `oc_users`,
`oc_accounts`, `oc_groups`, `oc_group_user`. **Skip**: `oc_sessions`, `oc_jobs`,
`oc_authtoken.last_activity/last_check` columns (updated per request), any
`*_queue`. Warn on unknown tables rather than silently ignoring.

## 4. Scenarios (data-driven YAML)

A scenario = ordered list of typed ops (`put`, `get`, `mkcol`, `move`, `copy`,
`delete`, `propfind`, `proppatch`, `chunked_upload_v2`, `bulk`, `share_create`)
with method/path/headers/body refs to `fixtures/`. The runner replays identically
against both base URLs and also diffs **normalized responses** (status + selected
headers/body, minus `ETag`/`Date`).

Initial set (native ops first): `10_put_get_delete`, `11_mkdir_nested`,
`12_move_rename`, `13_copy`, `14_propfind_depth1`, `15_proppatch_favorite_tags`,
`16_overwrite_put` (copy-on-write path), `17_delete_to_trash`,
`18_explicit_mtime` (send `X-OC-Mtime` so mtime-preservation is checked
deterministically despite timestamp masking), `20_chunked_upload_v2`,
`21_bulk_upload`, `22_invalid_filename` (rejection parity), `23_quota_exceeded`,
`24_checksum_upload`, `30_share_create_selfcheck` (proxied → validates harness).
**Phase 2:** a randomized differential fuzzer generating op sequences over a small
alphabet, seeded, with failure-shrinking.

## 5. Quiescing nondeterminism
- Both instances already run `occ background:cron` at install (no AJAX-cron on
  requests) — confirm in preconditions.
- Snapshots in `REPEATABLE READ`; before/after taken close together.
- Mask per-request columns (`oc_authtoken.last_activity`); skip `oc_jobs`/`oc_sessions`.
- Previews require Imaginary: either configure the *same* Imaginary for both
  instances or scope preview scenarios to DB-row *shape* (skip generated bytes).
- Run each scenario twice; a diff that appears only on the second run flags
  residual flakiness to investigate, not to mask.
- **File tree**: compare `data/{user}/files/**` by relative path+size+sha256 via
  `docker exec master-nextcloud-1` / `master-oracle-1`; exclude volatile
  `files_versions/`, `cache/`, `appdata_*/`, in-flight `*.part`.

## 6. Phasing (get a green end-to-end slice first, then broaden)
1. **Stand up + sanity**: add the `oracle` service + proxy vhost (see *Key
   facts*), `make diff-up`; curl a WebDAV PUT against both `:8080` (SUT) and
   `:9091` (Oracle); confirm DBs `nextcloud`/`oracle` both exist and report the
   same `oc_version` and enabled-app set.
2. **Pipeline skeleton**: crate scaffold, `client.rs`, `db.rs` snapshot,
   minimal `canonicalize.rs`/`delta.rs`/`report.rs`. Get `10_put_get_delete`
   green end-to-end (validates the whole chain).
3. **Full canonicalization**: natural-key id-bijection + equality-preserving
   masking + complete `column_registry.yaml`; add `fs.rs`; unit-test the
   canonicalizer. Add core native scenarios (11–18).
4. **Breadth**: chunked/bulk/quota/invalid-name/checksum/previews (20–24);
   proxied self-check (30).
5. **Fuzzer** (phase 2) + **CI gate** + `make diff-test`.

## 7. Risks & mitigations
- **Over/under-masking** → unit-test `canonicalize.rs` with hand-built fixtures;
  prefer preserving relationships over blanket masking; keep the registry explicit.
- **Two-instance config drift** (apps/caching/version/skeleton) → resolved by
  construction: the Oracle is a clone of the `nextcloud` service (same local
  php84 image, env, `additional.config.php`, skeleton), **not** the stock
  `nextcloud2` image, whose own bootstrap/env would install a different app set
  and caching backend even at the same PHP major. Residual drift (enabled-app
  set, redis/cache state) is still caught by the precondition checks, which fail
  fast.
- **fileid sequence offset** → natural-key bijection (the design centerpiece).
- **Residual background writes** → §5 quiescing + double-run flake detection.
- **Rust's thinner diff ecosystem** → hand-roll carefully, test it, use `similar`.

## Verification
- `make diff-up` → both `http://127.0.0.1:8080` (SUT) and `:9091` (Oracle)
  answer `GET /status.php` with `installed:true`; `psql -h 127.0.0.1 -p 8212`
  lists both `nextcloud` and `oracle`.
- `cargo test -p nc-difftest` (unit, un-gated) — canonicalizer/delta fixtures pass.
- `make diff-test` — all scenarios report **identical** on a known-good build.
- **Negative control** (proves the harness catches bugs): temporarily introduce a
  deliberate divergence in `nc-dav` (e.g. skip the `oc_filecache_extended` insert
  on PUT, or don't bump the parent etag) → the corresponding scenario must **fail**
  with a precise delta; revert. This guards against a harness that passes silently.
- Self-check scenario `30_share_create_selfcheck` (pure PHP on both sides) must
  always pass — if it fails, the harness, not the server, is wrong.
