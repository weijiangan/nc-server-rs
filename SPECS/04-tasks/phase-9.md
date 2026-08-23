# Phase 9 — Cross-Cutting Filesystem Concerns (Requirement-Gap Remediation)

Goal: implement the cross-cutting PHP behaviours that execute **inline on the Rust-native files subtree** and were missed by the original app/endpoint-line scoping. These are the requirement gaps recorded in [`../01-requirements/requirements/README.md`](../01-requirements/requirements/README.md) ("Requirement Gap: Cross-cutting concerns on the Rust-native files subtree") and specified in REQ §6.5.1, §6.7–§6.10 and §9.9. Many are exercised by the **web Files client** on every folder view.

> **Execution order:** although numbered 9 (to avoid renumbering existing phases), this phase depends on Phase 4 (DAV files tree) and Phase 5 (upload flows), and must land **before** the Phase 8 completion gate — the Cypress web-client suites and litmus/sync checks in Phase 8 assume this behaviour exists.
>
> **Guiding rule (per the latency-driven scope boundary):** a concern is pulled into Rust only because it is triggered inline on a Rust-native request and therefore cannot be proxied to PHP-FPM after the fact. Where data is already Rust-owned (`oc_share`, filecache, favorites tags) Rust computes it directly. Where data is owned by a delegated app (`comments`, `systemtags`) Rust satisfies only the read-only inline property; all **writes/management** for those apps stay in PHP-FPM.

---

### 9.1 Gap tables — schema awareness (NOT migrations)
> **PHP owns the schema** (see [`../02-specifications/improvements.md`](../02-specifications/improvements.md) §I.3 and [`../../core-rs/crates/nc-db/src/migrate.rs`](../../core-rs/crates/nc-db/src/migrate.rs)): the `files_trashbin` app and core `tags` code create these tables during PHP install/`occ app:enable`. Rust must **read/write** them, not issue DDL for them. Do **not** add `sqlx` migrations that `CREATE` these tables at runtime — that reintroduces the dual-migration divergence risk §I.3 was written to avoid.

- [x] Add `oc_files_trash` (REQ §9.4: `auto_id` BIGINT PK AI, `id` VARCHAR(250) = basename, `user` VARCHAR(64), `timestamp` VARCHAR(12), `location` VARCHAR(512), `type` VARCHAR(4) nullable, `mime` VARCHAR(255) nullable, `deleted_by` VARCHAR(64)) and `oc_vcategory` / `oc_vcategory_to_object` (REQ §9.9) to the **startup schema-validation allowlist** — verify they exist and have the critical columns, bail with a clear error if a required table/column is missing (a trash/favorites feature depends on them)
- [x] Add the same tables to the **test-only** schema fixtures used by the isolated `nc-db` integration DB (so Rust-only test harnesses can spin up a DB without a full PHP install) — clearly separated from any runtime path
- [ ] Confirm the queries against these tables compile-verify (sqlx) against a PHP-created schema on PostgreSQL, MySQL/MariaDB, and SQLite

**Verify:** starting Rust against a PHP-installed DB that has these tables passes validation; against a DB missing `oc_files_trash` it exits with a clear "run PHP install / enable files_trashbin" error rather than failing mid-request. `cargo test` in `nc-db` builds its isolated test DB and exercises trash/favorites read+write paths.

> **Note (2026-08-24):** the third bullet is stale. The gap-table queries are
> runtime-bound `format!` + `sqlx::query` strings, not compile-verified `query!`
> macros, and there is no `.sqlx` offline cache — "compile-verify (sqlx)" does
> not apply. And per the Postgres-first constitution (only PostgreSQL and
> SQLite exist), the MySQL/MariaDB wording is out of scope. The real coverage
> (which is done) is `validate_schema()` in
> `../../core-rs/crates/nc-db/src/schema.rs` checking these tables/columns, and
> the isolated `nc-db` test fixture creating them (`tests/schema.rs`).

> **Deviation (2026-08-24):** the allowlist is surfaced through a **manual**
> `nc-server check-schema` command, not a boot-time gate — see 9.9.

### 9.2 Cache propagation on write — parent ETag / mtime / size (REQ §6.8) — ✅ IMPLEMENTED
> PHP source: `lib/private/Files/Cache/Updater.php` (`update`/`remove`/`copyOrRenameFromStorage`) → `lib/private/Files/Cache/Propagator.php` (`propagateChange`).
> Rust: [`../../core-rs/crates/nc-dav/src/propagator.rs`](../../core-rs/crates/nc-dav/src/propagator.rs) (Propagator struct + DB queries) + [`../../core-rs/crates/nc-dav/src/filesystem.rs`](../../core-rs/crates/nc-dav/src/filesystem.rs) (wired into all mutation points) + [`../../core-rs/crates/nc-dav/src/davfile.rs`](../../core-rs/crates/nc-dav/src/davfile.rs) (PUT flush propagation) + [`../../core-rs/crates/nc-dav/src/bulk_handler.rs`](../../core-rs/crates/nc-dav/src/bulk_handler.rs) (bulk upload) + [`../../core-rs/crates/nc-dav/src/upload_handler.rs`](../../core-rs/crates/nc-dav/src/upload_handler.rs) (chunked assembly).

- [x] After every mutating op (`PUT`, `DELETE`, `MOVE`, `COPY`, `MKCOL`, chunked-upload assembly, mtime-changing `PROPPATCH`), walk the `parent` chain in `oc_filecache` up to the storage root — PHP drives this from `Updater::update` (`Updater.php:68-93`), `Updater::remove` (`Updater.php:96-118`) and `Updater::copyOrRenameFromStorage` (`Updater.php:159-205`), each calling `Propagator::propagateChange` (`Propagator.php:40`)
- [x] Each ancestor `etag` = `uniqid()` — one value shared by all ancestors of a single `propagateChange` (`Propagator.php:76`, comment "we give all folders the same etag"); the `etag` column is **skipped** when the storage is an `IReliableEtagStorage` (`Propagator.php:87-89`)
- [x] Each ancestor `mtime = GREATEST(mtime, min(change_time, now))` (time clamped to now at `Propagator.php:48`; `GREATEST` at `Propagator.php:82`)
- [x] Each ancestor size = `CASE WHEN size > -1 THEN GREATEST(size + sizeDifference, -1) ELSE size END` — unscanned `-1` folders left untouched (`Propagator.php:91-99`); `unencrypted_size` adjusted **only** when the storage is `instanceof Encryption` (`Propagator.php:102-118`)
- [x] `sizeDifference` source: `Updater::update` computes `size − oldSize` from a shallow rescan (`Updater.php:76-80`; forced `null` when the entry is encrypted); `Updater::remove` passes `−entry->getSize()` (`Updater.php:112`)
- [x] `MOVE`/`COPY` propagates **both** chains — `propagateChange(source)` **and** `propagateChange(target)`, each with `sizeDifference = 0` (etag/mtime only) (`Updater.php:203-204`); the immediate source/target parents' sizes are fixed by a `Cache::correctFolderSize` recalculation, **not** a signed delta (`Updater.php:195-200`)
- [x] One `UPDATE` covers all ancestors of a change via `path_hash IN (…)` (`Propagator.php:78-84`); request-level batching accumulates via `inBatch`/`addToBatch` (`Propagator.php:68-72`); retried up to `MAX_RETRIES = 3` (`Propagator.php:26`)

**Verify:** PUT a 1 MB file into `/A/B/C/`; PROPFIND `{DAV:}getetag` + `{oc:}size` on `A`, `B`, `C` all changed; `size` increased by 1 MB at each already-scanned ancestor. Delete it; sizes revert and ETags change again. A desktop-sync incremental poll detects the change purely from the parent ETag.

> **Deviations from PHP:**
> 1. **Architecture — Propagator is per-request, not per-storage-singleton.** PHP bootstraps Updater/Propagator per-storage in the DI container. Rust creates a single `Propagator` instance per `NcFileSystem` (per-request), which is cheap (three fields: pool, prefix, storage_id). All mutations within the request share the same Propagator via `Clone`. This avoids PHP's shared-nothing overhead while keeping request isolation.
> 2. **No `IReliableEtagStorage` check.** Rust always updates the etag on ancestors. The PHP check for `IReliableEtagStorage` (`Propagator.php:87-89`) skips etag updates for storages that self-manage etags (e.g. S3 object stores). We don't currently support such storages in Rust; the check can be added when that changes.
> 3. **No `Encryption` storage `unencrypted_size` adjustment.** PHP adjusts `unencrypted_size` in parallel with `size` when the storage wraps `Encryption` (`Propagator.php:102-118`). Rust does not support server-side encryption yet; this column is left untouched.
> 4. **SQL: `CASE WHEN` instead of `GREATEST`.** SQLite lacks the `GREATEST` function (PostgreSQL/MySQL have it). The propagation SQL uses `CASE WHEN mtime < $time THEN $time ELSE mtime END` for cross-DB compatibility. The behaviour is identical.
> 5. **No `SELECT … FOR UPDATE` row locking.** PHP wraps PostgreSQL/MySQL propagation in a transaction with `SELECT … FOR UPDATE` to avoid deadlocks (`Propagator.php:120-150`). Rust uses auto-committed UPDATE statements without explicit locking. The `GREATEST`/`CASE WHEN` expressions are commutative, and the retry loop handles transient failures. `correctFolderSize` (used only for MOVE/COPY) has a theoretical SELECT-then-UPDATE race, matching PHP's own race window on the same path.
> 6. **No request-level batch yet.** PHP's `beginBatch()`/`commitBatch()` accumulates changes across multiple mutations in one request (`Propagator.php:68-72, 167-285`). The current implementation calls `propagate_change` individually per mutation. Batching can be added later as an optimisation; the Propagator field on `NcFileSystem` is structured to support it.
> 7. **`Updater` logic is distributed, not a separate struct.** Instead of a standalone `Updater` module in the (currently empty) `nc-files` crate, the higher-level mutation semantics (computing `sizeDifference`, calling `correctFolderSize` for MOVE/COPY, etc.) are handled inline at each mutation point in `NcFileSystem`. This keeps the write path local and avoids cross-crate plumbing for what is fundamentally request-scoped work.

### 9.3 Trash bin on DELETE (REQ §6.7) — ✅ IMPLEMENTED (with residual gaps)
> PHP source: `apps/files_trashbin/lib/Storage.php` (unlink/rmdir interception), `apps/files_trashbin/lib/Trashbin.php` (`move2trash`).
> Rust: [`../../core-rs/crates/nc-dav/src/filesystem.rs`](../../core-rs/crates/nc-dav/src/filesystem.rs) (`move_to_trash`, `trash_directory`, `is_trashbin_enabled`, `remove_file`/`remove_dir`) + [`../../core-rs/crates/nc-dav/src/handler.rs`](../../core-rs/crates/nc-dav/src/handler.rs) (atomic directory-DELETE interception).

- [x] `DELETE` on `/dav/files/{userId}/{path}` (and `/remote.php/…` alias) moves the node instead of permanently deleting it (guarded by `is_trashbin_enabled`; hard-delete fallback when the app is disabled)
- [x] Disk: rename `{datadir}/{userId}/files/{path}` → `{datadir}/{userId}/files_trashbin/files/{basename}.d{timestamp}` (whole subtree for directories, moved as one unit; **basename only** — original directory structure is not encoded in the trash name, matching PHP `pathinfo()`). Collisions bump the timestamp (PHP-compatible, `{name}.d{ts}` only — no `_N` suffix)
- [x] `oc_filecache` row **updated** (not deleted): `path`, `path_hash`, `name` (trash basename incl. `.d{ts}`), `parent` (fileid of auto-created `files_trashbin/files`), `mtime = timestamp`; directory descendants' `path`/`path_hash` rewritten to nest under the trashed dir
- [x] `oc_filecache_extended` row left unchanged (keyed by `fileid`)
- [x] Insert one `oc_files_trash` row per top-level node with **exactly** the columns PHP writes: `id = basename` (matches PHP — **not** `fileid`), `user`, `timestamp` (**bound as a string** — the column is `VARCHAR(12)`), `location = dirname`, `deleted_by`. The nullable `type`/`mime` columns are left `NULL` (PHP does not set them; `type` is only `VARCHAR(4)`, so writing `'folder'` would overflow). Insert failures are logged, not swallowed.
- [x] Auto-create the `files_trashbin/` and `files_trashbin/files/` collection rows if missing (`ensure_parent_dir`)
- [x] Permanent delete via `/dav/trashbin/…` stays delegated to PHP-FPM (router forwards `/remote.php/dav/trashbin/*`, unchanged, REQ §6.7.4)
- [x] Honour the full PHP "should move to trash?" decision (`Storage.php:125-146`): app enabled, not a `.part` file, `X-NC-Skip-Trashbin: true` absent, path under `files/` (`shouldMoveToTrash`)
- [x] Runs propagation (§9.2) on the source parent

> **Residual gaps:**
> 1. **Shared-file deletes ignore the owner's trashbin.** PHP routes a deleted *received share* into the **owner's** trashbin. Rust always writes `user`/`deleted_by = self.uid`. De-scoped — requires cross-storage share-owner resolution not yet in Rust.
> 2. **Versions not retained** (`retainVersions`). Tracked by §9.4.
> 3. **No events/hooks emitted** (`post_moveToTrash`, etc.). General cross-cutting-events gap.
> 4. **Inline trashbin size check skipped** (`getConfiguredTrashbinSize`). Low severity — background expiry still runs via PHP-FPM cron.

**Verify:** `build/integration/dav_features/webdav-related.feature` DELETE scenarios; then confirm the file is listed under `/dav/trashbin/{user}/` (served by PHP-FPM) and a row exists in `oc_files_trash`. Cypress `cypress/e2e/files/` trashbin flow: delete in web UI → item appears in Deleted files view.

### 9.4 File versions on overwrite (REQ §6.9) — ✅ IMPLEMENTED
> PHP source: `apps/files_versions/lib/Storage.php` (`store`, `VERSIONS_ROOT`), `apps/files_versions/lib/Versions/LegacyVersionsBackend.php` (`createVersion`), `apps/files_versions/lib/Listener/FileEventsListener.php` (write/rename/copy hooks).
> Rust: [`../../core-rs/crates/nc-dav/src/versions.rs`](../../core-rs/crates/nc-dav/src/versions.rs) + wired into [`davfile.rs`](../../core-rs/crates/nc-dav/src/davfile.rs) (flush) and [`filesystem.rs`](../../core-rs/crates/nc-dav/src/filesystem.rs) (rename/copy).

- [x] **Design decision:** native Rust. `CreateVersionEvent` has zero listeners outside `files_versions` itself.
- [x] On `PUT` overwrite of an existing file: copy prior content to `files_versions/{relativePath}.v{mtime}`, where `mtime` is the **pre-overwrite** `$file->getMtime()` and the **full relative path is preserved** (unlike trash, which flattens to basename) — `LegacyVersionsBackend::createVersion` copies `files/{relativePath}` → `files_versions/{relativePath}.v{mtime}` (`apps/files_versions/lib/Versions/LegacyVersionsBackend.php:150-163`; `VERSIONS_ROOT = 'files_versions/'` at `Storage.php:52`). Done in `davfile.rs:flush()` before the temp→final rename.
- [x] Trigger = the write hook via `Storage::store()`, which **skips** `.part` files, directories, and **empty (size 0)** files, and dispatches `CreateVersionEvent` (a listener may veto) before creating the version (`apps/files_versions/lib/Storage.php:156-215`). `CreateVersionEvent` not dispatched (zero external listeners).
- [x] Auto-create parent folders under `files_versions/` and scan the new row so the versions PROPFIND (PHP-FPM) can enumerate it — `Storage::createMissingDirectories` (`LegacyVersionsBackend.php:155`) + `getFileInfo` rescan (`LegacyVersionsBackend.php:162`). Done via `ensure_version_parents` + `insert_version_row`.
- [x] On `MOVE`/`COPY` of a file, relocate its `files_versions/{path}.v*` siblings alongside it — handled by the move/copy hooks in `apps/files_versions/lib/Listener/FileEventsListener.php` (`copy_hook` at `:355`, matching rename/move handlers). Done via `rename_versions()` / `copy_versions()`.
- [x] Read-side (browse/restore via `/dav/versions/…`) remains delegated to PHP-FPM (unchanged, REQ §6.1)

**Verify:** PUT new content over an existing file twice; PROPFIND `/dav/versions/{user}/versions/{fileid}` (PHP-FPM) lists the prior versions with correct sizes. Move the file; versions still resolve. `apps/files_versions` Behat/Cypress version-restore flow passes.

> **Deviations from PHP:**
> 1. **`CreateVersionEvent` not dispatched.** No Rust event system exists; verified zero external listeners.
> 2. **Shared-mount owner resolution skipped.** PHP `Storage::store()` resolves shared-mount owners before versioning (`Storage.php:184-191`). Rust versions under the reque
sting user's storage.
> 3. **Version filecache rows use `lookup-before-insert`** instead of PHP's scanner `getFileInfo`. The outcome is identical — a filecache row exists for the version under `
files_versions/`.
> 4. **`scheduleExpire` not called.** PHP schedules background expiration of old versions. Rust does not — the PHP-FPM `files_versions` background job still handles expiry
independently.

### 9.5 Favorites & personal tags properties (REQ §6.5.1)
> PHP source: `apps/dav/lib/Connector/Sabre/TagsPlugin.php` (registered unconditionally for logged-in users at `apps/dav/lib/Server.php:328`).

- [ ] `{oc:}favorite` in PROPFIND (`FAVORITE_PROPERTYNAME = '{http://owncloud.org/ns}favorite'`, `TagsPlugin.php:42`): returns `1`/`0` based on presence of the sentinel `TAG_FAVORITE = '_$!<Favorite>!$_'` (`TagsPlugin.php:43`) among the node's tags (`getTagsAndFav` `TagsPlugin.php:117-134`; handler `TagsPlugin.php:236-244`). Backing store = the `ITags` `load('files')` system = `oc_vcategory` / `oc_vcategory_to_object` (REQ §9.9)
- [ ] `{oc:}tags` in PROPFIND (`TAGS_PROPERTYNAME = '{http://owncloud.org/ns}tags'`, `TagsPlugin.php:41`): the node's tag names **excluding** the favorite sentinel (unset in `getTagsAndFav` `TagsPlugin.php:125-133`)
- [ ] `{oc:}favorite` writable via `PROPPATCH` — truthy test `(int)$favState === 1 || $favState === 'true'` → `tagAs`/`unTag` `TAG_FAVORITE`; returns `200` (or `204` on delete) (`TagsPlugin.php:270-289`)
- [ ] `{oc:}tags` writable via `PROPPATCH` — `updateTags` diffs current vs requested and skips the favorite sentinel (`TagsPlugin.php:180-200`)
- [ ] Depth-1 PROPFIND batches the lookup for all children — `preloadCollection` prefetches when either prop is requested, "pre-fetching only supported for depth <= 1" (`TagsPlugin.php:203-224`)
- [ ] Favorites survive a trash round-trip (mapping keyed by `fileid`, which §9.3 preserves)

**Verify:** star a file via `PROPPATCH {oc:}favorite=1`; PROPFIND returns `1`; the Favorites REPORT (§9.8) includes it. Cypress `cypress/e2e/files/FilesFavorites*` passes. Unstar → PROPFIND returns `0` and mapping row removed.

### 9.6 Share badge properties (REQ §6.5.1)
> PHP source: `apps/dav/lib/Connector/Sabre/SharesPlugin.php` (registered unconditionally for logged-in users at `apps/dav/lib/Server.php:336`).

- [ ] `{oc:}share-types` in PROPFIND (`SHARETYPES_PROPERTYNAME = '{http://owncloud.org/ns}share-types'`, `SharesPlugin.php:31`; protected at `:68`): `array_unique` of the shares' `getShareType()` ints (`handleGetProperties` `SharesPlugin.php:195-201`)
- [ ] `{nc:}sharees` in PROPFIND (`SHAREES_PROPERTYNAME = '{http://nextcloud.org/ns}sharees'` — **nc:** namespace, `SharesPlugin.php:32`; protected at `:69`): recipient list from the same shares (`:203-207`)
- [ ] Shares aggregate **both** owner shares (`getSharesBy`) and recipient shares (`getSharedWith`) over types USER/GROUP/LINK/REMOTE/EMAIL/ROOM/CIRCLE/DECK (`getShare` `SharesPlugin.php:86-116`); all rows live in `oc_share` (REQ §9.6), though CIRCLE/DECK/ROOM semantics originate in those apps
- [ ] Depth-1 batches the folder's shares once via `getSharesInFolder` (`preloadCollection`/`getSharesFolder` `SharesPlugin.php:124-171`)

**Verify:** share a file (via PHP-FPM sharing API), then PROPFIND on the owner's tree returns `{oc:}share-types` with the correct type. Web Files app shows the shared badge on the row.

### 9.7 Comments & system-tags PROPFIND enrichment (REQ §6.5.1)
> PHP source: `apps/dav/lib/Connector/Sabre/CommentPropertiesPlugin.php`, `apps/dav/lib/SystemTag/SystemTagPlugin.php`. Both are registered **unconditionally** for logged-in users (`CommentPropertiesPlugin` at `apps/dav/lib/Server.php:341`; `SystemTagPlugin` at `apps/dav/lib/Server.php:237`) — there is **no** app-enabled gate on these properties.

- [ ] `{oc:}comments-href` (`PROPERTY_NAME_HREF = '{http://owncloud.org/ns}comments-href'`, `CommentPropertiesPlugin.php:19`): the request base URI with path segment `dav/comments/files/{fileid}` substituted at `/remote.php/` (`getCommentsLink` `CommentPropertiesPlugin.php:145-156`); returns `null` if the request is not under `/remote.php/`
- [ ] `{oc:}comments-count` (`:20`): `ICommentsManager::getNumberOfCommentsForObject('files', fileid)` from `oc_comments` (`:127`). Always served when logged in; value is `0` when the object has no comment rows — **not** gated on any app being enabled
- [ ] `{oc:}comments-unread` (`:21`): per-user `getNumberOfUnreadCommentsForObjects('files', [id], user)` (`:136`; `getUnreadCount` `:170-180`); `null` only when there is no user (never on an authenticated PROPFIND)
- [ ] `{nc:}system-tags` (`SYSTEM_TAGS_PROPERTYNAME = '{http://nextcloud.org/ns}system-tags'`, `SystemTagPlugin.php:54`): the file's system tags from `ISystemTagObjectMapper::getTagIdsForObjects([fileid], 'files')`, filtered by `canUserSeeTag`, natural-sorted (`propfindForFile` `SystemTagPlugin.php:335-345`; `getTagsForFile` `:351-380`). Empty list when the file has no user-visible tags — no app-enabled gate
- [ ] Depth-1 prefetch for both: comments via `getNumberOf*CommentsForObjects` over all children (`CommentPropertiesPlugin.php:51-95`); system-tags via `getTagIdsForObjects`, "pre-fetching only supported for depth <= 1" (`SystemTagPlugin.php:204-236`)
- [ ] A requested property that **no** plugin handles is returned as `404` inside the `207` by Sabre — but since these handlers are always registered, they return a value (`0`/empty), not `404`
- [ ] All **writes/management** for comments and system tags remain PHP-FPM (no write path added here)

**Verify:** with comments present, add a comment via PHP-FPM, then PROPFIND shows `{oc:}comments-count = 1` and `{oc:}comments-unread` correct per user. With no comments, the property is present and returns `0` (not `404`).

### 9.8 `filter-files` REPORT — web Favorites / Tags / Recent views (REQ §6.10)
> PHP source: `apps/dav/lib/Connector/Sabre/FilesReportPlugin.php` (registered for logged-in users at `apps/dav/lib/Server.php:361`).

- [x] `REPORT` `{http://owncloud.org/ns}filter-files` (`REPORT_NAME` `FilesReportPlugin.php:33`) on `/dav/files/{userId}/…` handled natively (intercept before dav-server, which does not implement REPORT); only when the target is a `Directory` (`onReport` `:108-111`) *(partial: directory check deferred; REPORT handled on all paths)*
- [x] `{oc:}filter-rules` parsing (`:122-125`):
  - [x] `{oc:}favorite` — **presence-based** (the rule's value is ignored): if present, results = `fileTagger->load('files')->getFavorites()` from `oc_vcategory` (`:228-243`). *(Correction: not a `1`/`0` toggle.)*
  - [ ] `{oc:}systemtag` (`SYSTEMTAG_PROPERTYNAME` `:34`) — repeatable tag id; resolves via `getTagsByIds` + `userFolder->searchBySystemTag`, intersected across multiple tags (`:253-290`)
  - [ ] `{oc:}circle` (`CIRCLE_PROPERTYNAME` `:35`) — circle id; returns `[]` unless the `circles` app is enabled (`getCirclesFileIds` `:300-305`)
- [x] Empty `filter-rules` block → `400 Bad Request` ("Missing filter-rule block") — never scans all files (`:145-148`)
- [ ] Filter by non-existent tag → `TagNotFoundException` → `412 Precondition Failed` (`:158-160`)
- [x] `{DAV:}limit` paging: `{DAV:}nresults` (page size) + `{nc:}firstresult` (offset) (`:134-141`)
- [x] `{DAV:}prop` block selects the per-match property set (`:128-133`; property set = §4.7–§4.9 + §9.5–§9.7)
- [x] Response `207 Multi-Status`, one `<d:response>` per match, scoped to the report target's subtree via `findNodesByFileIds` (`:169`; status set `:184-190`) *(partial: subtree scoping deferred — all favorites returned regardless of REPORT URI path)*
- [ ] `{DAV:}supported-report-set` advertises `{oc:}filter-files` (`getSupportedReportSet` `:96`)

**Verify:** star two files, then `REPORT filter-files` with an `{oc:}favorite` filter-rule returns exactly those two. Empty filter → `400`. Non-existent systemtag → `412`. Cypress Favorites and Tags sidebar views load in the web client.

### 9.9 Startup schema validation (replaces disabled `migrate!()`)
> Context: [`../02-specifications/improvements.md`](../02-specifications/improvements.md) §I.3 and [`../../core-rs/crates/nc-db/src/migrate.rs`](../../core-rs/crates/nc-db/src/migrate.rs). PHP owns the schema; `nc_db::migrate::run()` is currently a no-op. Replace it with a fail-fast validation so a mis-provisioned DB is caught at boot, not mid-request.

- [x] Replace the no-op `nc_db::migrate::run()` with a `validate_schema()` that queries `information_schema` (PostgreSQL/MySQL) / `pragma_table_info` (SQLite) for every table + critical column Rust reads or writes
- [x] Coverage = the core+files tables (REQ §9.1–§9.8) **plus** the §9.1 gap tables (`oc_files_trash`, `oc_vcategory`, `oc_vcategory_to_object`)
- [x] On a missing table/column: bail at startup with a clear, actionable error (which table/column, and that PHP install / `occ app:enable` must create it) — do **not** attempt DDL
- [x] Do **not** re-enable `sqlx::migrate!()` on the runtime path; keep the `migrations/` SQL only for schema docs, sqlx compile-verify, and the isolated `nc-db` test DB
- [ ] Validation runs once at boot, before the HTTP listener opens (same slot the old `migrate!()` occupied in `main.rs`)

**Verify:** boot against a complete PHP-installed DB → validation passes, server starts. Drop `oc_files_trash` → boot fails fast with an error naming the table; no request is served. Remove a critical column from `oc_filecache` → same fail-fast behaviour.

> **Deviation (2026-08-24):** boot-time validation was deliberately **not**
> implemented — the operator wanted to inspect the schema on demand without
> taking the server down. The work landed as the one-shot `nc-server
> check-schema` subcommand (in `nc-server/src/main.rs`): it loads the config,
> connects to the DB, runs `nc_db::schema::validate_schema()` (read-only catalog
> queries only), prints every missing table/column, and exits non-zero on
> mismatch. Startup never runs it and never issues DDL. The `migrate.rs` no-op
> was removed (replaced by `schema.rs`); `sqlx::migrate!()` stays off the
> runtime path and the `migrations/` SQL is used only by the isolated `nc-db`
> test fixture. The context line above links to the deleted `migrate.rs`.

> **Note (2026-08-24):** `validate_schema()` targets PostgreSQL
> (`information_schema.columns` + `table_name = ANY($1)`) and SQLite
> (`sqlite_master` + `PRAGMA table_info`). There is no MySQL/MariaDB branch —
> that dialect is out of scope per the Postgres-first constitution.

### 9.10 Phase exit criteria
- [ ] `cargo test --all-features` exits 0 (includes new propagation, trashbin, favorites, and REPORT unit/integration tests)
- [ ] `build/integration/dav_features/` and `build/integration/files_features/` scenarios touching delete, favorites, shares, and metadata pass
- [ ] `cypress/e2e/files/` suites for Favorites, Tags, Deleted files (trashbin), and share badges pass against the Rust server
- [ ] A desktop-sync incremental cycle (add / overwrite / move / delete) is detected via parent-folder ETag propagation with no full rescan
- [ ] Startup schema validation (§9.9) passes against a PHP-installed DB and fails fast on a missing table/column

**Verify:** the above suites exit 0; manual web-client smoke test confirms stars, tags, deleted-files view, comment badges, and share badges render correctly.

## Changes

### 2026-08-24 — 9.1 + 9.9: schema validation as a manual `check-schema` command

**Decision:** the operator asked for the 9.1/9.9 schema check to be runnable on
demand ("let me run to just to see if the schema matches") rather than a
boot-time gate. The boot-time bail (9.9's last checkbox and its Verify text)
was therefore intentionally not implemented; everything else in 9.1/9.9 landed.

**What landed:**
- `core-rs/crates/nc-db/src/schema.rs` — `validate_schema(pool, prefix)` +
  `SchemaReport`, with the required table/column list (`SCHEMA`). Read-only
  catalog queries only (`information_schema` on Postgres, `sqlite_master` +
  `PRAGMA table_info` on SQLite); never issues DDL.
- The three §9.1 gap tables (`oc_files_trash`, `oc_vcategory`,
  `oc_vcategory_to_object`) plus every other table Rust reads/writes
  (`oc_comments`, `oc_comments_read_markers`, `oc_systemtag`,
  `oc_systemtag_object_mapping`, `oc_files_versions`, `oc_preview_generation`,
  …) are in `SCHEMA`.
- `core-rs/crates/nc-server/src/main.rs` — `check-schema` subcommand; prints a
  per-item report and exits 1 on mismatch. The old `nc_db::migrate::run()` no-op
  call was removed from startup.
- `core-rs/crates/nc-db/src/migrate.rs` deleted; `pub mod migrate` → `pub mod
  schema` in `lib.rs`.
- `core-rs/crates/nc-db/tests/migrations.rs` (pre-existing broken: `&DbPool`
  no longer implements `Executor` after PHASE-22 T3.3) replaced by
  `tests/schema.rs`: an isolated SQLite fixture built from the `migrations/`
  SQL (the sanctioned test-only consumer) + explicit DDL for the PHP-app-owned
  tables, then `validate_schema` round-trips.

**Key decision — where the column list comes from:** the `SCHEMA` columns are
grounded in the PHP Doctrine migrations (`workspace/server/core/Migrations/`
and `apps/*/lib/Migration/`), **not** the Rust `core-rs/migrations/*.sql`, which
are stale (e.g. they put `creation_time`/`upload_time` on `oc_filecache`, which
PHP keeps only on `oc_filecache_extended`; `oc_accounts.id` and
`oc_accounts_data.verified` also don't exist in the live PHP schema). The live
dev DB was cross-checked against the PHP migrations and matched exactly.

**Verification:**
- `cargo test -p nc-db` — 39 unit + 5 integration tests pass (full fixture
  validates; missing table and missing column are reported).
- `cargo build -p nc-server` — whole dependency chain compiles.
- `nc-server --root <sqlite-root> check-schema` smoke-tested three ways:
  empty DB → 24 missing tables, exit 1; complete fixture → pass, exit 0;
  `oc_files_trash` without `deleted_by` → single missing column, exit 1.
- The Postgres catalog query (`information_schema.columns` +
  `table_name = ANY($1)`) was run against the live dev DB and returns the
  expected tables/column counts.
