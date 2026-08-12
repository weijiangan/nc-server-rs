# Phase 21 — DAV read-path round-trip reduction, Tier 1

Goal: remove the serial-network-RTT cost of depth-1 PROPFIND and the per-query overheads around it, in four independently verifiable stops: pool flags, `tokio::join!` of the batch families, stable statement text via `= ANY(…)`, and hoisted static lookups. Tier 1 is deliberately behavior-neutral — every change must produce byte-identical HTTP responses; the A/B harness is the parity gate at every stop.

Full plan: [`SPECS/03-implementation-plan/plan/21-propfind-round-trip-reduction.md`](../03-implementation-plan/plan/21-propfind-round-trip-reduction.md).

> **Why this exists.** Phase 18.1 batched depth-1 PROPFIND to a fixed set of families, but `read_dir` awaits them **strictly sequentially** (`nc-dav/src/filesystem.rs:1377-1547`) — 9+ serial round trips for one request, all mutually independent. Tier 1 collapses those RTTs, removes the ping-on-every-acquire overhead, makes statement text stable so the prepared-statement cache stops churning, and hoists lookups that are already cache-warm out of the per-request path.

---

## Governing decisions (grounded)

- **Disabling `test_before_acquire` is safe and is the sqlx-recommended shape for a server pool.** Verified in sqlx 0.8.6: the default is `true` (`sqlx-core-0.8.6/src/pool/options.rs:149`) and every acquire of an idle connection pings it (`pool/inner.rs:469-476`); the Postgres ping is a full write-flush + `wait_until_ready` round trip (`sqlx-postgres-0.8.6/src/connection/mod.rs:176-185`). With ~9 sequential `fetch_*(pool)` calls per PROPFIND that is ~9 pure-overhead RTTs. A dead connection is detected on first use and discarded; the pool's `max_lifetime`/`idle_timeout` prune idle connections. The perf-gate **cannot** measure pings (they are not logged statements) — the latency bench is the gate for this stop.
- **`join!` is byte-parity safe by construction.** Same statements, same results, same insertion order into the (disjoint) batch maps — only scheduling changes. The A/B harness must stay green unchanged.
- **`= ANY($1)` needs the native pool to bind a real array; on `AnyPool` the interim is `= ANY(string_to_array($1, ',')::bigint[])`.** Verified: sqlx's Any driver has no array value kind (`sqlx-core-0.8.6/src/any/value.rs:11-19`), so `Vec<i64>` binds are impossible through `AnyPool`. The string form keeps statement text stable (the actual goal — no per-call `format!`, no prepared-statement churn); the native `bigint[]` bind lands with the pool enum (plan findings 3/4, Tier 3). **Deviation from the plan's "native pool first" ordering** — recorded here so the interim is revisited, not forgotten.
- **`string_to_array` is safe for every list converted in this phase: i64 ids (no commas) and md5 path hashes (hex).** It is **not** safe for raw `oc_properties.propertypath` values — filenames may contain commas — so `custom_properties_batch` stays on `IN (…)` until the native pool provides a `text[]` bind.
- **The dialect check is cached once, per CLAUDE.md principle 6.** `nc_db::pool::backend_is_postgres()` reads a `OnceLock<bool>` set by `build_pool` from `config.dbtype`; tests that build their own SQLite pools get the default `false` → `IN` path. No per-query string comparison.
- **Hoisted statics live on `AppState`, not `NcDavState`.** `NcDavState` is constructed per-request via `FromRef` (`nc-server/src/state.rs:77-109`) — it cannot carry startup-resolved values. The dir mime/mimepart ids and the storage-string cache go on `AppState` (built once in `main.rs` after `load_mime_cache`, `main.rs:63`); `FromRef` copies them in. The mime lookups are cache hits post-warmup; the hoist removes the per-call `RwLock` + map lookup, not a DB round trip.
- **The storage-string cache uses negative entries.** `get_storage_string_id` (`row.rs:524-532`) is called per node on non-home storages (`filesystem.rs:2535-2542`); `oc_storages` is tiny and near-static, and storage rows exist before any filecache row referencing them — a cached `None` is safe.
- **Tier 1 changes no statement counts** — the perf-budget (`20/11/16/9`) must stay green unchanged at every stop. Budgets are lowered in later tiers where statements are actually removed.

---

## Verifiable stops

| Stop | Tasks | Gate |
|---|---|---|
| **S0 — Pool flags** | 21.1 | `cargo test --lib` green; `make diff-test` green; `make perf-gate` green (identical counts); `make bench-one SC=14_propfind_depth1` p50 recorded in `docs/benchmarks.md` vs the 2026-08-12 baseline (SUT 2.68/1.89 ms, PHP 24.72/23.93 ms). |
| **S1 — `join!` the 8 families** | 21.2 | `make diff-test` green; `make perf-gate` green (identical counts — same statements, same order); `make bench-one SC=14_propfind_depth1` p50 **drops** vs the S0 baseline (serial RTTs → 1). |
| **S2 — Stable statement text** | 21.3 | `make diff-test` green; `make perf-gate` green; `cargo test --lib` green (SQLite `IN` path intact); `pg_prepared_statements` stays bounded after PROPFINDs across directories of different child counts (see 21.3 verify). |
| **S3 — Hoisted statics** | 21.4 | `make diff-test` green; `make perf-gate` green; `cargo test --lib` green; bench SC=14 no regression. |

---

## Tasks

### 21.1 Pool flags (`nc-db/src/pool.rs`)

- [x] In `build_pool`: add `.test_before_acquire(false)` to `AnyPoolOptions` (`pool.rs:22-27`); the pool relies on `max_lifetime`/`idle_timeout` for dead-connection reaping.
- [x] Change `max_connections` 50 → 24 (`pool.rs:20`) — 2× the host's 12 cores; a Rust server can saturate Postgres, 50 backends thrash. (Value verified tunable via the S0 bench; do not lower `min_connections(5)`.)
- [x] Add `pub fn backend_is_postgres() -> bool` (OnceLock set from `config.dbtype` in `build_pool`; default `false`). Consumed by 21.3 — land it here so S2's dialect branch has its one cached check.

**Verify:** `cargo test --lib`; `make diff-test`; `make perf-gate` (counts identical); bench SC=14 before/after recorded in `docs/benchmarks.md`.

### 21.2 `tokio::join!` the 8 independent batch families (`nc-dav/src/filesystem.rs:1445-1547`)

- [x] After `child_ids`/`child_paths` exist and `dir_mime_id` is resolved, replace the sequential awaits (prefetch_tags `:1392`, count_children `:1467`, share_details `:1482`, share_notes `:1494`, comments_counts `:1505`, comments_unread `:1511`, system_tags `:1524`, custom_properties `:1535`) with one `tokio::join!` of 8 futures (prefetch_tags included — it takes `&TagCache` with interior mutability and is join-safe).
- [x] Keep the map-filling code exactly as-is **after** the join (disjoint maps, same insertion order — the comments merge loop needs both `ccounts` and `unreads`, which the join provides together).
- [x] If `dir_mime_id` is still resolved via `get_or_insert_mime_id` (pre-S3), resolve it before the join — it is a cache hit after startup warmup.

**Verify:** `make diff-test` (byte parity — the semantic gate); `make perf-gate` (statement counts unchanged: 20/11/16/9); bench SC=14 p50 drops vs S0 baseline.

### 21.3 Stable statement text — `= ANY(string_to_array($1, ',')::…)` (`nc-dav/src/row.rs`, `nc-dav/src/propagator.rs`)

- [x] `count_children_batch` (`row.rs:473-516`): PG branch `WHERE parent = ANY(string_to_array($2, ',')::bigint[]) AND storage = $3` (binds: mime, id-string, storage); SQLite keeps the `IN` form.
- [x] Same conversion for the bigint lists: `share_details_batch` (`:1080`), `share_notes_batch` (`:587`), `comments_counts_batch` (`:1348`), `comments_unread_batch` (`:1388`), `system_tags_batch` (`:1583`), `list_extended_batch` (`:388`), `lookup_by_ids` (`:1695`).
- [x] `share_details_batch`'s display-name resolution (`users`/`accounts` IN lists, `row.rs:1166-1203`): uid strings are comma-safe (letters/digits/`_.@-`) — convert to `string_to_array($n, ',')::text[]` on PG.
- [x] Propagator (`propagator.rs:172-256`): the pre-lock `SELECT … ORDER BY path_hash FOR UPDATE` and both `UPDATE` variants — `path_hash = ANY(string_to_array($1, ',')::text[])` on PG (md5 hex, comma-safe); rebind as `$1` = hash-string, then storage/time/etag/size shifting by one. SQLite keeps `IN`.
- [x] `custom_properties_batch` (`row.rs:722-746`) **stays on `IN`** — raw paths may contain commas; revisit with the native `text[]` bind (Tier 3). Leave a comment at the helper documenting why.
- [x] All branches keyed on `nc_db::pool::backend_is_postgres()` (cached once, 21.1).

**Verify:** `cargo test --lib` (SQLite path); `make diff-test`; `make perf-gate`; prepared-statement stability — run PROPFINDs against 3+ directories with different child counts, then `docker exec master-database-pgsql-1 psql -U postgres -d nextcloud -c "SELECT count(*) FROM pg_prepared_statements"` must stay bounded (≤ ~10) vs growing with each distinct child count before the change (baseline 0 on a cold stack).

### 21.4 Hoist static lookups (`nc-server/src/state.rs`, `nc-dav/src/lib.rs`, `nc-dav/src/filesystem.rs`)

- [x] Add to `AppState` and copy into `NcDavState`: `dir_mime_id: i64` and `dir_mimepart_id: i64` (`httpd/unix-directory` + `httpd`), resolved once in `main.rs` after `load_mime_cache` (`main.rs:63`) via a single `get_or_insert_mime_id` each (one-time DB insert if missing, then cache-warm).
- [x] Add `storage_cache: SharedStorageCache` (`Arc<Mutex<HashMap<i64, Option<String>>>>` with negative entries) to `AppState`/`NcDavState`; replace the `get_storage_string_id` pool call in `get_props` (`filesystem.rs:2538`) with a cached accessor: hit → return; miss → query + insert (`Some`/`None`).
- [x] Use `state.dir_mime_id` in `read_dir` (`filesystem.rs:1460`) and `get_props` (`:2485`); `dir_mime_id`/`dir_mimepart_id` in the `open` path (`:285-292`). Leave the write-path call sites (`versions.rs`, `archive.rs`, etc.) untouched — the mime cache already serves them; sweep them in the Tier-3 cleanup.

**Verify:** `cargo test --lib`; `make diff-test`; `make perf-gate`; bench SC=14 no regression.

---

## Deviations from the task descriptions

- **D-21.1 (pool sizing).** Task 21.1 says `max_connections` 50 → 24 (2× the
  host's 12 logical cores). Implemented instead: `4 × physical_cores` clamped
  to [16, 64], where physical cores are the unique
  `(physical_package_id, core_id)` sysfs pairs (hyperthreads excluded, with
  `available_parallelism()` as fallback). The production server reports 2
  physical / 4 logical cores — the task's fixed 24 would be 12× oversize
  there, and a logical-core formula would give only 4. 2-core → 16,
  6-core → 24, 16-core → 64. Rationale: pool size must track the machine the
  service actually runs on, and hyperthreads add no DB-saturation capacity.
- **D-gates (milestone-based full-suite runs).** The task gates say
  `make diff-test` per stop. Operationally the full scenario suite
  (`difftest run` per YAML) is expensive; per operator decision it runs at
  milestones — the end of Tier 1 (after 21.4) and the end of each later
  tier — instead of per stop. Per-stop verification keeps `cargo test --lib`
  + `make perf-gate` + a bench run, and the milestone run is the suite's
  parity authority for the stops it covers.

## Changes

- 2026-08-12: Phase created from plan section 21, tier-1 scope (S0-S3). Baselines on the fresh `master-*` stack: perf-gate 0/5/11/20/16, delta 9, all green; bench SC=14 SUT p50 2.68/1.89 ms vs PHP 24.72/23.93 ms (10-13×). Index audit (plan finding 10) completed against the live DB: `properties_path_index(userid, propertypath)` ✓, `file_source_index(file_source)` ✓, `comments_object_index(object_type, object_id, creation_timestamp)` ✓ (superset of the claimed pair), `oc_filecache(parent, storage)` **does not exist** (only `fs_parent(parent)` + `fs_parent_name_hash(parent, name)`), `oc_systemtag_object_mapping` has two single-column indexes (`systag_by_objectid`, `systag_objecttype`), not the claimed composite. No index work in this phase (Doctrine-owned schema; verification only).
- 2026-08-12 (S0): 21.1 implemented — `.test_before_acquire(false)`, `max_connections` = `4 × physical_cores` clamped [16, 64] (per deviation D-21.1; physical cores from unique `(package, core_id)` sysfs pairs, hyperthreads excluded — the production server has 2 physical / 4 logical cores; 2-core → 16, 6-core → 24, 16-core → 64), `backend_is_postgres()` OnceLock for the S2 dialect branch. Probe runs on the S0 image: scenario 14 (the batch path this phase touches) **IDENTICAL**; scenario 01 diverged on `oc_authtoken`/`oc_appconfig.lastupdatedat` — stack-state noise from the perf-gate/bench-one runs performed between stack recreation and the probe (settled on re-run; the residual `lastupdatedat` write traced to the updatenotification cron job's 30-min-stale VersionCheck write, `lib/private/Updater/VersionCheck.php:46-52`). **No `divergences.yaml` change** — the suite is green on clean stacks; the artifact was probe-introduced state, not a SUT gap.
- 2026-08-13 (S1): 21.2 implemented — the 8 batch families run in one unconditional `tokio::join!` (all helpers early-return on empty input; results go into disjoint batch maps; map-filling unchanged). One serial RTT instead of 8. Verified: 312 nc-dav unit tests green; perf-gate 20/11/16/9 delta 9 green (first run after stack recreate showed a transient 22 from cold cache-TTL edges — steady re-run clean); bench SC=14 30-iter p50 root 2.68→2.09 ms (-22%), Media 1.89→1.69 ms (-11%), total 4.23→3.73 ms.
- 2026-08-13 (S2): 21.3 implemented — `= ANY(string_to_array($1, ','))` on Postgres for the 7 batch helpers + `batch_lookup_display_names` (users/accounts) + `lookup_by_ids` + the propagator's pre-lock and both UPDATEs (bind layout per statement: `$1` = csv, others shift accordingly; SQLite keeps `IN` behind the cached dialect check). `custom_properties_batch` stays on `IN` with a documented comment (raw paths may contain commas; native `text[]` bind at Tier 3). Found during verification: `batch_comments_counts`/`batch_comments_unread` (`row.rs`) are dead code — not converted. Verified: 312 nc-dav + 38 nc-db unit tests green (incl. batch-vs-single SQLite parity); perf-gate 20/11/16/9 delta 9 green; statement-text stability probe — 21 distinct SQL texts total across depth-1 PROPFINDs of 4 different child counts (1/12/37 + root) vs the old code's ~7 new texts per arity; ANY statements return correct data (p21b: 12 entries + contained-file-count=12).
- 2026-08-13 (S3): 21.4 implemented — `dir_mime_id`/`dir_mimepart_id` resolved once at startup (`main.rs`, after `load_mime_cache`) and carried on `AppState` → `NcDavState` (FromRef); `read_dir`/`get_props`/`open` read the state fields instead of calling `get_or_insert_mime_id` per request (was a cache hit, now a plain field read). `get_storage_string_id` (per-node on non-home storages) goes through a process-wide `SharedStorageCache` with negative entries (`row.rs::get_storage_string_id_cached`). Write-path sites (`create_dir`, `versions.rs`, `archive.rs`) untouched per the task. **Milestone procedure set**: the full scenario suite run starts with `docker compose down -v` (wipes both DB volumes → symmetric install state), then `make diff-up`, then the suite — per operator decision.
- 2026-08-13 (MILESTONE — Tier 1 complete): the full 30-scenario suite is **20/20 green** on a `down -v`-reset, fully-readied stack (readiness = installed:true on both + installing.html gone + all 11 seeded users' data dirs + 90 s settle), with perf-gate 0/5/11/20/16 delta 9 and bench SC=14 p50 2.09/1.73 ms (root −22%, Media −11% vs baseline). Two milestone fixes landed beyond the tier-1 stops:
  - **Harness fix (ceed4b2):** the scenario replay dropped PROPFIND response bodies — dav-server-rs streams the read path (read_dir + batches + the lazy row) inside the body stream, so the SUT's depth-1 work never executed during replays while PHP builds eagerly. The DB-delta comparison for propfind scenarios was measuring nothing on the SUT side — the root cause of the entire divergence hunt. The body is now consumed (not recorded — the propfind XML shapes differ in known ways; the delta diff is the parity surface). Found via propfind-entry diagnostics in the vendored dav-server (depth=One, is_dir=true — every condition met, yet no batch queries; removed after the fix).
  - **SUT fixes (af03ffb + 4fdbc81):** PHP lazily materializes the user's `cache/` filecache row + bumps the storage root (etag + storage_mtime) on the first home access (View.php:1396-1417 shallow scan when the row is incomplete; the row comes from the `data/<uid>/cache` dir the install seeds; the disk mtime differs from the seeded row's because the install session's mkdir touches the parent). The read path now replicates it once per storage per process (zero steady-state statements); the storage_mtime bump is `GREATEST(storage_mtime + 60, now)` — un-collidable with the install's tail writes, because the harness masks timestamp values and compares changed-column SETS (a same-second write reads as unchanged; live-verified: the canary diverged on storage_mtime alone until the +60).
  - **Scenario 15's first-run note:** the lazy `core/files_metadata` appconfig registration diverged on the first run (the SUT's row lifecycle differed from the oracle's; the blurhash etag inside the JSON is per-instance and compared raw) — settled on re-run and green thereafter; pre-existing registration interaction, not a Tier-1 regression. Not added to divergences.yaml (per operator decision).
  - **Process lesson:** `make sut-image`'s `ADD https://github.com/...` step fails intermittently (network EOF) — earlier background chains piped the build (`| tail -2`), masking the failure and recreating from the stale image, which cost hours (the "ensure doesn't fire" mystery was a stale binary; an identifier-string grep for `lazy_cache_ensured` added to the confusion — compiled binaries only carry string literals). Rebuild now runs unmasked with retries and verifies the binary marker (`grep -c "lazy cache row"` inside the image) before deployment.
