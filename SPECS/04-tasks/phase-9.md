# Phase 9 — Cross-Cutting Filesystem Concerns (Requirement-Gap Remediation)

Goal: implement the cross-cutting PHP behaviours that execute **inline on the Rust-native files subtree** and were missed by the original app/endpoint-line scoping. These are the requirement gaps recorded in [`../01-requirements/requirements/README.md`](../01-requirements/requirements/README.md) ("Requirement Gap: Cross-cutting concerns on the Rust-native files subtree") and specified in REQ §6.5.1, §6.7–§6.10 and §9.9. Many are exercised by the **web Files client** on every folder view.

> **Execution order:** although numbered 9 (to avoid renumbering existing phases), this phase depends on Phase 4 (DAV files tree) and Phase 5 (upload flows), and must land **before** the Phase 8 completion gate — the Cypress web-client suites and litmus/sync checks in Phase 8 assume this behaviour exists.
>
> **Guiding rule (per the latency-driven scope boundary):** a concern is pulled into Rust only because it is triggered inline on a Rust-native request and therefore cannot be proxied to PHP-FPM after the fact. Where data is already Rust-owned (`oc_share`, filecache, favorites tags) Rust computes it directly. Where data is owned by a delegated app (`comments`, `systemtags`) Rust satisfies only the read-only inline property; all **writes/management** for those apps stay in PHP-FPM.

---

### 9.1 Gap tables — schema awareness (NOT migrations)
> **PHP owns the schema** (see [`../02-specifications/improvements.md`](../02-specifications/improvements.md) §I.3 and [`../../core-rs/crates/nc-db/src/migrate.rs`](../../core-rs/crates/nc-db/src/migrate.rs)): the `files_trashbin` app and core `tags` code create these tables during PHP install/`occ app:enable`. Rust must **read/write** them, not issue DDL for them. Do **not** add `sqlx` migrations that `CREATE` these tables at runtime — that reintroduces the dual-migration divergence risk §I.3 was written to avoid.

- [ ] Add `oc_files_trash` (REQ §9.4: `auto_id` BIGINT PK AI, `id` VARCHAR(250) = basename, `user` VARCHAR(64), `timestamp` VARCHAR(12), `location` VARCHAR(512), `type` VARCHAR(4) nullable, `mime` VARCHAR(255) nullable, `deleted_by` VARCHAR(64)) and `oc_vcategory` / `oc_vcategory_to_object` (REQ §9.9) to the **startup schema-validation allowlist** — verify they exist and have the critical columns, bail with a clear error if a required table/column is missing (a trash/favorites feature depends on them)
- [ ] Add the same tables to the **test-only** schema fixtures used by the isolated `nc-db` integration DB (so Rust-only test harnesses can spin up a DB without a full PHP install) — clearly separated from any runtime path
- [ ] Confirm the queries against these tables compile-verify (sqlx) against a PHP-created schema on PostgreSQL, MySQL/MariaDB, and SQLite

**Verify:** starting Rust against a PHP-installed DB that has these tables passes validation; against a DB missing `oc_files_trash` it exits with a clear "run PHP install / enable files_trashbin" error rather than failing mid-request. `cargo test` in `nc-db` builds its isolated test DB and exercises trash/favorites read+write paths.

### 9.2 Cache propagation on write — parent ETag / mtime / size (REQ §6.8)
> PHP source: `lib/private/Files/Cache/Updater.php` (`update`/`remove`/`renameFromStorage`) → `lib/private/Files/Cache/Propagator.php` (`propagateChange`).

- [ ] After every mutating op (`PUT`, `DELETE`, `MOVE`, `COPY`, `MKCOL`, chunked-upload assembly, mtime-changing `PROPPATCH`), walk the `parent` chain in `oc_filecache` up to the storage root
- [ ] Each ancestor: `etag` = fresh unique opaque value (one per propagation batch); skip for storages implementing reliable-etag semantics
- [ ] Each ancestor: `mtime = GREATEST(mtime, change_time)`
- [ ] Each ancestor: `size = size + sizeDifference`, applied **only** where `size > -1` (leave unscanned `-1` folders untouched); adjust `unencrypted_size` when encryption is active
- [ ] `MOVE`/`COPY` across folders propagates on **both** source parent (−size) and target parent (+size)
- [ ] Propagation batched per request and flushed once (single UPDATE pass, not N per ancestor)

**Verify:** PUT a 1 MB file into `/A/B/C/`; PROPFIND `{DAV:}getetag` + `{oc:}size` on `A`, `B`, `C` all changed; `size` increased by 1 MB at each already-scanned ancestor. Delete it; sizes revert and ETags change again. A desktop-sync incremental poll detects the change purely from the parent ETag.

### 9.3 Trash bin on DELETE (REQ §6.7)
> PHP source: `apps/files_trashbin/lib/Storage.php` (unlink/rmdir interception), `apps/files_trashbin/lib/Trashbin.php` (`move2trash`).

- [ ] `DELETE` on `/dav/files/{userId}/{path}` (and `/remote.php/…` alias) moves the node instead of permanently deleting it
- [ ] Disk: rename `{datadir}/{userId}/files/{path}` → `{datadir}/{userId}/files_trashbin/files/{path}.d{timestamp}` (whole subtree for directories)
- [ ] Collision: append `_1`, `_2`, … to the `.d{timestamp}` suffix
- [ ] `oc_filecache` row **updated** (not deleted): `path`, `path_hash`, `name` (+`.d{ts}`), `parent` (fileid of auto-created `files_trashbin/files`), `mtime = timestamp`
- [ ] `oc_filecache_extended` row left unchanged (keyed by `fileid`)
- [ ] Insert one `oc_files_trash` row per node: `id=fileid`, `user`, `timestamp`, `location=files/{path}`, `type` (`file`/`folder`), `deleted_by`
- [ ] Auto-create the `files_trashbin/` and `files_trashbin/files/` collection rows if missing
- [ ] Runs propagation (§9.2) on the source parent
- [ ] Permanent delete via `/dav/trashbin/…` stays delegated to PHP-FPM (unchanged, REQ §6.7.4)

**Verify:** `build/integration/dav_features/webdav-related.feature` DELETE scenarios; then confirm the file is listed under `/dav/trashbin/{user}/` (served by PHP-FPM) and a row exists in `oc_files_trash`. Cypress `cypress/e2e/files/` trashbin flow: delete in web UI → item appears in Deleted files view.

### 9.4 File versions on overwrite (REQ §6.9)
> PHP source: `apps/files_versions/lib/AppInfo/Application.php` (Node written/deleted/renamed/copied listeners), `apps/files_versions/lib/Storage.php`, `apps/files_versions/lib/Listener/FileEventsListener.php`, `.../VersionStorageMoveListener.php`.

- [ ] **Design decision (blocking):** implement version copy natively in Rust (mirrors §9.3) **or** expose a synchronous internal hook the PHP shim implements. Preferred: native, to keep the write path off PHP-FPM. Record the decision here before implementing.
- [ ] On `PUT` overwrite of an existing file (when `files_versions` is enabled): before writing new content, copy prior content to `{datadir}/{userId}/files_versions/{path}.v{old_mtime}`
- [ ] Create/maintain `oc_filecache` rows under `files_versions/` (auto-create folder) so the versions PROPFIND (PHP-FPM) can enumerate them
- [ ] On `MOVE`/`COPY` of a file, relocate its `files_versions/{path}.v*` siblings alongside it
- [ ] Read-side (browse/restore via `/dav/versions/…`) remains delegated to PHP-FPM (unchanged, REQ §6.1)

**Verify:** PUT new content over an existing file twice; PROPFIND `/dav/versions/{user}/versions/{fileid}` (PHP-FPM) lists the prior versions with correct sizes. Move the file; versions still resolve. `apps/files_versions` Behat/Cypress version-restore flow passes.

### 9.5 Favorites & personal tags properties (REQ §6.5.1)
> PHP source: `apps/dav/lib/Connector/Sabre/TagsPlugin.php`.

- [ ] `{oc:}favorite` in PROPFIND: `1` when a `(fileid, favorite-category)` mapping exists in `oc_vcategory_to_object` (category `_$!<Favorite>!$_`, type `files`, owner = current user), else `0`
- [ ] `{oc:}tags` in PROPFIND: list of the node's non-favorite personal tag names
- [ ] `{oc:}favorite` writable via `PROPPATCH` — star/unstar inserts/removes the mapping row (auto-create the favorite category row per user on first use)
- [ ] `{oc:}tags` writable via `PROPPATCH` — reconcile the node's tag set
- [ ] Depth-1 PROPFIND batches the tag lookup for all children (single query, no per-node round trip)
- [ ] Favorites survive a trash round-trip (mapping keyed by `fileid`, which §9.3 preserves)

**Verify:** star a file via `PROPPATCH {oc:}favorite=1`; PROPFIND returns `1`; the Favorites REPORT (§9.8) includes it. Cypress `cypress/e2e/files/FilesFavorites*` passes. Unstar → PROPFIND returns `0` and mapping row removed.

### 9.6 Share badge properties (REQ §6.5.1)
> PHP source: `apps/dav/lib/Connector/Sabre/SharesPlugin.php`.

- [ ] `{oc:}share-types` in PROPFIND: list of share-type ints present on the node, derived from `oc_share` (already Rust-owned, REQ §9.6). Protected (read-only).
- [ ] `{nc:}sharees` in PROPFIND: recipient list from `oc_share`. Protected.
- [ ] Depth-1 batches the `oc_share` lookup for all children in one query

**Verify:** share a file (via PHP-FPM sharing API), then PROPFIND on the owner's tree returns `{oc:}share-types` with the correct type. Web Files app shows the shared badge on the row.

### 9.7 Comments & system-tags PROPFIND enrichment (REQ §6.5.1)
> PHP source: `apps/dav/lib/Connector/Sabre/CommentPropertiesPlugin.php`, `apps/dav/lib/SystemTag/SystemTagPlugin.php`.

- [ ] `{oc:}comments-href` = static `…/dav/comments/files/{fileid}` (no query needed)
- [ ] `{oc:}comments-count` = count from `oc_comments` (read-only query); `0` when the `comments` app is disabled
- [ ] `{oc:}comments-unread` = per-user unread from `oc_comments` + read markers (read-only); `0` when disabled
- [ ] `{nc:}system-tags` = list from `oc_systemtag` + `oc_systemtag_object_mapping` (read-only); omitted when the `systemtags` app is disabled
- [ ] Disabled-app / not-requested properties return `404` inside the `207` multistatus (matches PHP)
- [ ] All **writes/management** for comments and system tags remain PHP-FPM (no write path added here)

**Verify:** with `comments` enabled, add a comment via PHP-FPM, then PROPFIND shows `{oc:}comments-count = 1` and `{oc:}comments-unread` correct per user. Disable the app → property returns `404` in the multistatus, not `500`.

### 9.8 `filter-files` REPORT — web Favorites / Tags / Recent views (REQ §6.10)
> PHP source: `apps/dav/lib/Connector/Sabre/FilesReportPlugin.php`.

- [ ] `REPORT` with body `{http://owncloud.org/ns}filter-files` on `/dav/files/{userId}/…` handled natively (intercept before dav-server, like §4.11 sync-collection)
- [ ] `{oc:}filter-rules` parsing: `{oc:}favorite` (`1`/`0` → `oc_vcategory`), `{oc:}systemtag` (repeatable tag id), `{oc:}circle` (circle id; may be empty when circles disabled)
- [ ] Empty `filter-rules` block → `400 Bad Request` (never scan all files)
- [ ] Filter by non-existent tag → `412 Precondition Failed`
- [ ] `{DAV:}limit` paging: `{DAV:}nresults` (page size) + `{nc:}firstresult` (offset)
- [ ] `{DAV:}prop` block selects the per-match property set (§4.7–§4.9 + §9.5–§9.7)
- [ ] Response `207 Multi-Status`, one `<d:response>` per match, scoped to the report target's subtree
- [ ] `{DAV:}supported-report-set` on collections advertises `{oc:}filter-files`

**Verify:** star two files, then `REPORT filter-files` with `{oc:}favorite=1` returns exactly those two. Empty filter → `400`. Non-existent systemtag → `412`. Cypress Favorites and Tags sidebar views load in the web client.

### 9.9 Startup schema validation (replaces disabled `migrate!()`)
> Context: [`../02-specifications/improvements.md`](../02-specifications/improvements.md) §I.3 and [`../../core-rs/crates/nc-db/src/migrate.rs`](../../core-rs/crates/nc-db/src/migrate.rs). PHP owns the schema; `nc_db::migrate::run()` is currently a no-op. Replace it with a fail-fast validation so a mis-provisioned DB is caught at boot, not mid-request.

- [ ] Replace the no-op `nc_db::migrate::run()` with a `validate_schema()` that queries `information_schema` (PostgreSQL/MySQL) / `pragma_table_info` (SQLite) for every table + critical column Rust reads or writes
- [ ] Coverage = the core+files tables (REQ §9.1–§9.8) **plus** the §9.1 gap tables (`oc_files_trash`, `oc_vcategory`, `oc_vcategory_to_object`)
- [ ] On a missing table/column: bail at startup with a clear, actionable error (which table/column, and that PHP install / `occ app:enable` must create it) — do **not** attempt DDL
- [ ] Do **not** re-enable `sqlx::migrate!()` on the runtime path; keep the `migrations/` SQL only for schema docs, sqlx compile-verify, and the isolated `nc-db` test DB
- [ ] Validation runs once at boot, before the HTTP listener opens (same slot the old `migrate!()` occupied in `main.rs`)

**Verify:** boot against a complete PHP-installed DB → validation passes, server starts. Drop `oc_files_trash` → boot fails fast with an error naming the table; no request is served. Remove a critical column from `oc_filecache` → same fail-fast behaviour.

### 9.10 Phase exit criteria
- [ ] `cargo test --all-features` exits 0 (includes new propagation, trashbin, favorites, and REPORT unit/integration tests)
- [ ] `build/integration/dav_features/` and `build/integration/files_features/` scenarios touching delete, favorites, shares, and metadata pass
- [ ] `cypress/e2e/files/` suites for Favorites, Tags, Deleted files (trashbin), and share badges pass against the Rust server
- [ ] A desktop-sync incremental cycle (add / overwrite / move / delete) is detected via parent-folder ETag propagation with no full rescan
- [ ] Startup schema validation (§9.9) passes against a PHP-installed DB and fails fast on a missing table/column

**Verify:** the above suites exit 0; manual web-client smoke test confirms stars, tags, deleted-files view, comment badges, and share badges render correctly.
