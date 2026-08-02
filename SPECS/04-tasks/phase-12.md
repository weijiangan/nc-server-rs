# Phase 12 — PROPFIND Response Parity

Goal: the Rust-native `PROPFIND` response for the files tree is byte-for-byte identical to PHP's where it matters to sync clients (iOS, Android, desktop). The Depth:0 root response serves as the protocol handshake that tells the client whether to proceed to Depth:1 listing. Missing or malformed fields stall the client.

Byte-for-byte parity has three structural requirements, in priority order:

1. **Emit exactly the requested properties.** PHP (SabreDAV `PropFind::handle()`) only ever fills properties present in the client's `<d:prop>` list. Rust's `build_props()` currently returns every property unconditionally.
2. **Correct 200/404 propstat grouping.** Requested properties with no value go into a second `<d:propstat>` with `<d:status>HTTP/1.1 404 Not Found</d:status>` — never as empty strings or `0` in the 200 propstat (see 12.1).
3. **Correct values** for each individual property (12.2–12.13).

> **Status (2026-07-31): RESOLVED** — the iOS app (34.0.0) now authenticates and lists files against Rust (`PROPFIND Depth:0 → Depth:1 → files`), verified against the **local dev PHP** on the same database. The actual gate was mixed-case XML namespace prefixes in the PROPFIND response (see *Resolution* at the bottom). Most of the 2026-07-28 analysis below was built from a **stale cold-start capture** (`cloud.home.lan`/`cloud2.home.lan`, since discarded as a reference) and is superseded there; deviations from the original task descriptions are noted under each task.

## Discovery (2026-07-28 — live differential testing with iOS 34.0.0)

The iOS client would authenticate, receive a correct Depth:0 PROPFIND with root properties, but **never proceed to Depth:1** — it saw an incomplete response and considered the server incompatible. Cross-referencing the Rust response against PHP's on the same database revealed the gaps below.

**Wire captures** (kept as ground truth for this phase): Depth:0 PROPFIND pairs from `cloud.home.lan` (PHP) and `cloud2.home.lan` (Rust), 2026-07-28, same database/instance (`ocecf7uk5jlr`, root fileid 79558, iOS client 34.0.0). Where this spec cites the captures, that is the evidence. *(Superseded as ground truth, 2026-07-31: these are a different/stale production environment — `cloud.home.lan` a cold-start snapshot, `cloud2` stale Rust — and are no longer a verification target; use the local A/B dev docker, see the Status banner and Resolution. They remain cited below only as the original discovery context for PHP property shape/behavior.)*

**Environment note `[ENV]`:** the capture server runs apps that the master reference deployment does not — `text` and `files_lock` are absent from `workspace/server/` and not enabled on master (verified via `oc_appconfig`). Tasks marked `[ENV]` cannot be verified against the reference tree; for the master environment their correct behavior is a 404 propstat (subsumed by 12.1), not an empty element.

### Fixed (this session)

| Gap | Rust | PHP | Fix |
|-----|------|-----|-----|
| `DAV` response header | Missing | `1, 3, extended-mkcol, access-control, calendarserver-principal-property-search, nc-paginate, nextcloud-checksum-update, nc-calendar-search, nc-enable-birthday-calendar` | Added static header matching PHP's SabreDAV output. An earlier revision appended the trailing `, 2` (Class 2 Locking) to match the production capture; removed on 2026-07-31 (`62b55fd`) — Rust does not implement DAV Class 2. |
| `d:getetag` quoting | `6a4f0c6fd3bf6` | `"6a4f0c6fd3bf6"` | `DavMetaData::etag()` (`crates/nc-dav/src/metadata.rs`) now wraps in double quotes per RFC 4918 §8.8, matching `Node::getETag()` (`apps/dav/lib/Connector/Sabre/Node.php:184-186`) |

*(The previous "oc:etag quoting" row of this table was removed: PHP does not emit `oc:etag` at all. See 12.10.)*

### Still to do

Each entry below is a concrete, scoped task, verified against the PHP reference (`workspace/server/`) and/or the wire captures. Source citations are `file:line`. Each task opens with a prose description of the PHP reference behavior, followed by the actionable Rust work and verification as checkboxes.

---

### 12.1 Propstat discipline and request filtering *(structural — do first)*

**PHP:** SabreDAV fills only requested properties; anything requested but left unset lands in a 404 propstat (`3rdparty/sabre/dav/lib/DAV/CorePlugin.php` `httpPropFind`; `PropFind::handle()` is a no-op for unrequested names, null return ⇒ 404). The capture's root 404 propstat contains exactly: `nc:share-download-limits`, `d:getcontenttype`, `d:getcontentlength`, `oc:checksums`, `oc:downloadURL`, `nc:upload_time`, `nc:is-encrypted`, `nc:note`, `nc:lock-owner`, `nc:lock-owner-editor`, `nc:lock-owner-displayname`, `nc:lock-owner-type`, `nc:lock-time`, `nc:lock-timeout`, `nc:file-metadata-size`, `nc:file-metadata-gps`, `nc:metadata-photos-*` (5).

- [x] The Rust gap had two parts:
  - *(a) No 404 propstat.* The dav-server crate groups only OK-status elements; unhandled requested props are silently dropped, and `build_props()` emits empty/zero values where PHP leaves properties unset. Concrete divergences from the capture: `d:getcontenttype` on the directory (PHP 404; Rust 200 with `httpd/unix-directory`), `oc:checksums`/`oc:downloadURL`/`nc:note` (PHP 404; Rust empty 200), `nc:upload_time` on the directory (PHP 404 — `FilesPlugin.php:515-517` registers it inside the `File`-only branch; Rust 200 with `0`).
  - *(b) Request list ignored.* `build_props()` returns all properties regardless of the `<d:prop>` request. The Rust capture carries ~10 properties the client never asked for: `oc:etag`, `oc:tags`, `nc:is-mount-root`, `nc:is-federated`, `nc:hide-download`, `nc:contained-folder-count`, `nc:contained-file-count`, `nc:remind-me-at`, `nc:share-attributes`, `nc:acl-can-*`.
  - *(resolution)* dav-server vendored with patched propstat discipline (NEXTCLOUD-RS PATCH) — `core-rs/vendor/dav-server/src/handle_props.rs`; `build_props` conditionally emits only properties with values; driver props filtered to the requested set on explicit `<prop>` requests.
- [x] **Verify:** replay the captured iOS request; the Rust response must have the same 200-propstat member set and the same 404-propstat member set as PHP, with no extras. `nc:creation_time>0<` must remain in the 200 propstat (PHP emits `0` unconditionally — `FilesPlugin.php:446-448`; Rust's `0` is correct parity, not a gap).

> **Deviation (12.1):** the final vendored state is exactly two patches on upstream `v0.11.0`: `b9cd889` (this task's propstat discipline — 404 grouping, driver-prop filtering, driver-overrides-NOT_FOUND, collection `getcontenttype`→404, RFC 4918 §9.1) and `112b968` (lowercase `D:`→`d:` namespace prefix throughout — the actual iOS fix, see *Resolution*). The other SabreDAV-mirroring serialization tweaks tried on 2026-07-31 while hunting the gate — `cache-control: no-cache` removal, an empty-404-propstat guard, self-closing `<d:collection/>`, redundant-`xmlns` de-duplication, and dropping `encoding="UTF-8"` from the XML declaration — were **reverted and never committed**: none was the iOS blocker, and the crate stays a generic WebDAV server rather than copying SabreDAV/PHP output quirks.

### 12.2 `d:displayname` missing

**PHP:** `FilesPlugin.php:470-472` handles `{DAV:}displayname` with `$node->getName()` (the connector `Node::getName()` → `FileInfo::getName()`, `Node.php:104-106`). For the home root that value *is* the user's UID — capture shows `<d:displayname>6c21875f5c…</d:displayname>`. (The earlier draft of this task cited `getDisplayName()` — that is wrong; no fallback logic is involved.) For files and subdirectories it is the filename.

- [x] `NcMetaData.display_name` exists and is populated from `oc_filecache.name` (`crates/nc-dav/src/metadata.rs`), and feeds `DavDirEntry::name()`, but nothing emitted the `{DAV:}displayname` property — added to the prop set (subject to 12.1 request filtering).
- [x] **Verify:** Depth:0 PROPFIND on the home root includes `<d:displayname>` equal to the UID; a file node returns its filename.

### 12.3 `oc:permissions` — value mismatch, not letter logic

**PHP:** `DavUtil::getDavPermissions()` (`lib/public/Files/DavUtil.php:37-82`) is pure bit logic over `FileInfo::getPermissions()` (an unmasked cast of `oc_filecache.permissions`, `lib/private/Files/FileInfo.php:198-200`):

- `S` ⇔ `FileInfo::isShared()` (mount is `ISharedMountPoint`)
- `R` ⇔ `permissions & PERMISSION_SHARE` — **nothing else**; no shareability API, no mount-root suppression
- `M` ⇔ `FileInfo::isMounted()` (false for home storage, `FileInfo.php:270-273`)
- `G`/`D`/`V` ⇔ READ/DELETE/UPDATE bits
- `N` ⇔ `DavUtil::canRename()` (`DavUtil.php:84-102`) — note its special cases: always true for the root of a movable mount; always false for the home storage's `files` directory
- Files: `W` ⇔ writable (with a movable-mount-root check that reads the cache root entry directly, `DavUtil.php:62-70`); Directories: `CK` ⇔ CREATE bit

- [x] The letter mapping in `encode_permissions()` (`crates/nc-dav/src/props.rs`) already matches PHP's. The capture divergence (PHP `GDNVCK` vs Rust `RGDNVCK` for the *same fileid on the same database*) meant the two sides fed **different permission values** into identical logic. First hypothesis: PHP's `SetupManager::setupBuiltinWrappers()` wraps storages with a `sharing_mask` `PermissionsMask(mask=15)` when sharing is disabled for the user (`ShareDisableChecker::sharingDisabledForUser()`); the mask applies at the cache layer (`CachePermissionsMask`), so every `oc_filecache` read returns permissions with the SHARE bit stripped, while Rust read `oc_filecache.permissions` directly. Addressed by adding `sharing_disabled_for_user()` + `apply_sharing_mask()` in `row.rs` — but see the deviation below: this was the secondary path, not the capture's cause.
- [x] **Verify:** permission strings match PHP for a matrix of nodes: home root, ordinary directory, file, received-share root, subfolder inside a share — including the `N` special cases.

> **Deviation (2026-07-31):** the capture divergence this task was built on (PHP `GDNVCK`/`15` vs Rust `RGDNVCK`/`31` at the home root) was a **cold-start artifact**, not PHP's steady-state behavior. It is nonetheless genuinely **reproducible** in PHP: when `Root::getUserFolder()` runs before the user's filesystem is set up (`isSetupComplete` false — cold OPCache, post-restart, or first touch of the user folder), it returns an *unresolved* `LazyUserFolder` whose constructor caches `permissions = PERMISSION_ALL ^ PERMISSION_SHARE = 15` (`lib/private/Files/Node/LazyUserFolder.php`); `LazyFolder::getPermissions()` returns that cached `15` until the folder is resolved, after which the home root reports `RGDNVCK` / `31` / `["share","read","write"]` — the steady state, verified live via the local A/B harness. Rust targets the steady state (it reads the resolved `oc_filecache` row and cannot observe the lazy window): the unconditional `& !16` SHARE strip added here was **reverted**, and the regression tests that pinned `home-root=15` were corrected (the pure letter-encoding tests remain valid). The `sharing_mask` replication named above (`sharing_disabled_for_user()` + `apply_sharing_mask()` in `row.rs`) is **retained as a secondary path** — it only bites when sharing is disabled via `shareapi_exclude_groups`, and was never what produced the capture's `15`. The 2026-07-30 "canonical" trace explained the cold-start window, not the actual iOS gate (which was XML-serialization; see *Resolution*).

### 12.4 `ocs:share-permissions` / OCM `share-permissions` semantics

**PHP:** `Node::getSharePermissions()` (`apps/dav/lib/Connector/Sabre/Node.php:235-276`): inside a shared storage ⇒ the share's permissions; otherwise the node's own permissions, with `DELETE|UPDATE` OR-ed in for the root of a non-moveable, non-readonly mount. Capture: `15`. `{http://open-cloud-mesh.org/ns}share-permissions` maps that through `FilesPlugin::ncPermissions2ocmPermissions()` to JSON — capture: `["read","write"]` (`FilesPlugin.php:338-348`).

- [x] `ocs:share-permissions` defaulted to **31** for the owner's unshared files (MAX over `oc_share`, fallback 31 — `props.rs`, `row.rs`) instead of deriving from the node's permissions, and the OCM-namespaced property was not emitted at all — now derived from the node's permissions and emitted.
- [x] **Verify:** root emits `31` (matching live steady-state PHP — the `15` in the capture was the cold-start artifact; see 12.3), shared nodes emit the share's permission mask, and the OCM property appears with the JSON mapping.

> **Deviation (2026-07-31):** follows the 12.3 correction — the home root is **not** SHARE-stripped in steady state, so it emits `ocs:share-permissions=31` → OCM `["share","read","write"]` via `compute_share_permissions()` + `permissions_to_ocm_json()` (the `15` / `["read","write"]` pair the stale capture showed is gone). Both home-root and ordinary-directory values match live PHP; pinned by the corrected `row::tests::*pipeline*`. **Deferred:** the real-shared-storage case (where PHP uses the share's own mask via `Node::getSharePermissions`) — no shares in the fixture.

### 12.5 `oc:share-types` and `nc:sharees` missing

**PHP:** `SharesPlugin` (`apps/dav/lib/Connector/Sabre/SharesPlugin.php`): `{http://owncloud.org/ns}share-types` is *always* a 200-propstat value (handler never returns null) — an empty self-closing `<oc:share-types/>` when there are none (capture confirms). Children are `<oc:share-type>int</oc:share-type>` (`ShareTypeList.php:69-73`). Shares queried are both **by** and **with** the requesting user (`getSharesBy` + `getSharedWith`, lines 87-118) across types USER(0), GROUP(1), LINK(3), EMAIL(4), REMOTE(6), CIRCLE(7), ROOM(10), DECK(12). `{http://nextcloud.org/ns}sharees` is emitted from the same plugin (lines 208-212, `ShareeList`). For Depth:1, PHP preloads per directory with one `getSharesInFolder` call (lines 166-185).

- [ ] Neither property is emitted (the `share_type IN (0,1,3)` clauses in `row.rs` are unrelated query filters).
- [ ] **Verify:** root emits empty `<oc:share-types/>` and empty `nc:sharees`; a shared file emits the correct type integers and sharee entries. Implementation note: batch the share query per listed directory for Depth:1 (constitution 2).

### 12.6 `oc:comments-href` / `oc:comments-count` / `oc:comments-unread` missing

**PHP:** `CommentPropertiesPlugin` (`apps/dav/lib/Connector/Sabre/CommentPropertiesPlugin.php`) — *not* a class in the comments app. Three `{http://owncloud.org/ns}` properties, emitted for files **and** directories (line 123): `comments-count` (always an int, line 127), `comments-href` (derived from the base URI, lines 143-152), `comments-unread` (per requesting user; `null` ⇒ 404 when logged out, else integer including `0`, lines 135-137/158-166). Capture: `<oc:comments-unread>0</oc:comments-unread>`. Directory listing preloads counts in batch (`getNumberOfCommentsForObjects` / `getNumberOfUnreadCommentsForObjects`, lines 52-95).

- [ ] None of the three are emitted.
- [ ] **Verify:** root emits `comments-unread=0`, a valid `comments-href`, and `comments-count`; a commented file shows nonzero counts.

### 12.7 `nc:system-tags` missing

**PHP:** `OCA\DAV\SystemTag\SystemTagPlugin` (`apps/dav/lib/SystemTag/SystemTagPlugin.php:54,337-346`) — *not* `\OCA\SystemTags\SabrePlugin`. Handles `{http://nextcloud.org/ns}system-tags` for file nodes; always returns a `SystemTagList` (empty ⇒ self-closing `<nc:system-tags/>`, as in the capture). Tags resolved via `ISystemTagObjectMapper::getTagIdsForObjects([fileid], 'files')` + `ISystemTagManager::getTagsByIds()`, filtered for user visibility/assignability, natural-sorted by name. `systemtags` **is** enabled on master, so this task is fully verifiable against the reference.

- [ ] Not queried or emitted.
- [ ] **Verify:** empty element when no tags; inline tag objects (see `SystemTagList.php` for the exact child structure) otherwise.

### 12.8 `nc:mount-type` value

**PHP:** `FilesPlugin.php:404-406` returns `MountPoint::getMountType()`: base implementation returns `''` (`lib/private/Files/Mount/MountPoint.php:268-270`) — home roots are empty, confirmed by the capture (`<nc:mount-type></nc:mount-type>`). `SharedMount` returns `'shared'` (`apps/files_sharing/lib/SharedMount.php:184-186`); `ExternalMountPoint` overrides for external storage (`apps/files_external/lib/Config/ExternalMountPoint.php:27`).

- [x] `build_props()` hardcoded `"local"` (`crates/nc-dav/src/props.rs:157`) — now derives the mount type.
- [x] **Verify:** home root emits empty string; shared mounts emit `shared` when that path is implemented.

### 12.9 `oc:owner-display-name` wrong value

**PHP:** `FilesPlugin.php:366-396`: for an authenticated request, the owner's real display name (`$owner->getDisplayName()`). Capture: `<oc:owner-display-name>Tan Siew Kin</oc:owner-display-name>`. (Public-share requests apply an account-publish-scope check and may return null ⇒ 404.)

- [x] Emitted the owner **UID** instead of the display name (visible in the Rust capture) — now resolved the display name the same way PHP does (user accounts table), not from `oc_filecache`.
- [x] **Verify:** root response shows the display name, not the UID.

### 12.10 `oc:etag` must be removed, not quoted

**PHP:** never emits `{http://owncloud.org/ns}etag` on the files endpoint — there is no registration for it anywhere in the reference tree, and it is absent from the PHP capture's propstat.

- [x] `build_props()` emitted `oc:etag` unconditionally (`crates/nc-dav/src/props.rs:118`), from the raw unquoted field (bypassing the quoting `etag()` method), and the client never requests it — removed. The earlier "Fixed" entry treating this as a quoting gap was wrong on both counts.
- [x] **Verify:** property absent from all responses.

### 12.11 `nc:rich-workspace` missing `[ENV]`

**PHP:** returns `<nc:rich-workspace></nc:rich-workspace>` (empty, 200 propstat) on the root — emitted by the Text app's rich-workspace plugin when the app is enabled. The Text app is **not in `workspace/server/` and not enabled on master**, so the emitting source cannot be verified here; for the master environment the correct parity behavior is a 404 propstat (handled by 12.1). Implementing the empty-element behavior is only correct for deployments with the Text app, and would need that app's source to replicate (workspace file = directory `README.md` content, or empty string).

- [x] **Decision:** target environment = master-only; close as subsumed by 12.1.

### 12.12 `nc:lock` / `nc:lock-owner*` / `nc:lock-time*` missing `[ENV]`

**PHP:** returns `<nc:lock></nc:lock>` in the 200 propstat and `nc:lock-owner`, `nc:lock-owner-editor`, `nc:lock-owner-displayname`, `nc:lock-owner-type`, `nc:lock-time`, `nc:lock-timeout` in the 404 propstat (unlocked file). The attribution in the earlier draft of this task was wrong — `apps/dav/lib/Connector/Sabre/LockPlugin.php` only acquires/releases locks around PUT; it registers **no** properties, and no `nc:lock*` registration exists anywhere in `workspace/server/`. These properties come from the `files_lock` app, which is absent from the reference tree and not enabled on master. Same target-environment decision as 12.11; for master, subsumed by 12.1 (404 propstat). If implemented later against a `files_lock` checkout: `nc:lock` is always 200 (empty when unlocked), the sub-elements are 200 only while a manual lock (`oc_file_locks`) is active.

- [x] **Decision:** target environment = master-only; subsumed by 12.1.

### 12.13 `Vary: Brief,Prefer` header

**PHP:** SabreDAV's `CorePlugin::httpPropFind` sets `Vary: Brief,Prefer` — no space after the comma — on every PROPFIND response (`3rdparty/sabre/dav/lib/DAV/CorePlugin.php:332`; also on REPORT at line 376). The earlier draft misattributed this to the BrowserPlugin and misquoted the value. Capture confirms `vary: Brief,Prefer`.

- [x] There was no `Vary` header anywhere in the DAV path — added.
- [x] **Verify:** present on all PROPFIND responses, exact value `Brief,Prefer`.

---

### Deferred findings from the 12.3 trace (2026-07-30)

Surfaced while confirming the `oc:permissions` mechanism. None is a live divergence on the home storage / master environment today; they are latent until shares, ACLs, or external mounts are served. Tracked so they are not rediscovered.

### 12.14 `N` flag: parent-CREATE fallback and movable-mount root *(latent)*

**PHP:** `DavUtil::canRename($info, $parent)` (`lib/public/Files/DavUtil.php:84-102`) is four-way: movable-mount root (`MoveableMount` + `internalPath === ''`) ⇒ true; else updateable ⇒ true; else home storage's `files` dir ⇒ false ("can't rename the users home"); else `isDeletable() && $parent->isCreatable()`. `getDavPermissions` is passed `$this->node->getParent()` precisely for that last case.

- [ ] `props.rs:90-92` computes `can_rename = meta.permissions & 2 != 0` (the updateable case only). It never reads the parent and omits the movable-mount-root short-circuit; the code comment admits the fallback is "not checked here."
- [ ] **Impact today:** none on the home storage — every home node carries UPDATE (dirs 31 / files 27), so the updateable case always wins and the captures' `N` flags match. Diverges only for a deletable-but-not-updateable node whose parent is creatable — i.e. restricted-permission nodes (shares/ACLs).
- [ ] **Fix direction:** thread the parent's CREATE permission into `build_props` / `encode_permissions` and add the movable-mount-root case. Fidelity hardening, not a behavior fix, until shares/ACLs exist.

### 12.15 Home-root permissions: hardcode vs derive *(latent)*

**PHP:** `LazyUserFolder` hardcodes `permissions = 15` and `getPermissions()` returns it *without resolving* the folder (`lib/private/Files/Node/LazyUserFolder.php:42`, `LazyFolder::getPermissions`). The home root reports 15 regardless of `oc_filecache.permissions`.

- [ ] `filesystem.rs::get_props` derives `apply_sharing_mask(db_perms) & !16`. For a normal home root (`db_perms = 31`) this equals 15, but the semantic differs — PHP *hardcodes* 15, Rust *derives* it. If a home root's `oc_filecache.permissions` were ever ≠ 31, the two would diverge.
- [ ] **Decision needed:** hardcode 15 at the mount root to mirror PHP exactly, or keep the derive and document the assumption that home roots are always `PERMISSION_ALL`. Low priority — home roots are `PERMISSION_ALL` in practice.

> **Deviation (2026-07-31):** the premise ("PHP hardcodes 15") describes a real but **transient** PHP state, reproducible under cold start: `LazyUserFolder` (`lib/private/Files/Node/LazyUserFolder.php:42`) caches `permissions = PERMISSION_ALL ^ PERMISSION_SHARE = 15` and `LazyFolder::getPermissions()` returns it *only while the folder is unresolved* — i.e. when `Root::getUserFolder()` is reached before `setupForUser` (cold OPCache / post-restart / first touch). Once resolved, PHP reports `31` in steady state (see the 12.3 deviation). **Resolved by keeping the derive:** the unconditional `& !16` home-root strip was removed from `filesystem.rs::get_props` (see `:1881`), which now applies only the conditional `apply_sharing_mask(meta.permissions, sharing_disabled)`. Home root = `31` / `RGDNVCK` in steady state, matching live PHP; Rust reads the resolved cache row and cannot observe the lazy window, so the hardcode-vs-derive question is moot. Pinned by `row::tests::home_root_permission_pipeline_strips_share_when_sharing_disabled` (conditional, not unconditional).

### 12.16 Share / external-mount permission paths *(deferred until native shares)*

Correct for the home storage today; will diverge once received shares / external mounts are served. Grouped because they share the "not a home mount" trigger:

- [ ] `S` / `M` flags: Rust hardcodes `is_shared = false` and derives `is_mounted` from the storage `string_id`. PHP uses `FileInfo::isShared()` (`ISharedMountPoint`) and `isMounted()`.
- [ ] `Node::getSharePermissions()` (`apps/dav/lib/Connector/Sabre/Node.php:235-276`): the federated-token short-circuit (`getShareByToken`), the `ISharedStorage` ⇒ share-mask branch, and the `MoveableMount` DELETE|UPDATE injection for non-readonly mount roots are not replicated (Rust uses the home-only `compute_share_permissions`).
- [ ] `W` movable-mount-root re-derivation (`DavUtil.php:62-70`, documented deviation improvements §I.9): PHP re-reads the storage root cache entry for a file mounted at its own root; single-file shares only.
- [ ] `View::getFileInfo` `MoveableMount`-root DELETE injection (`$data['permissions'] |= PERMISSION_DELETE` when `internalPath === ''`, `lib/private/Files/View.php`): affects share roots; home is not a `MoveableMount`.

---

## Changes

### dav-server vendored (2026-07-29)

`core-rs/vendor/dav-server` — clone of `messense/dav-server-rs`, branch `nextcloud-0.11.0` based on tag `v0.11.0` (verified byte-identical to the crates.io tarball), wired via `[patch.crates-io]`. **Final state: exactly two patches on upstream** — `b9cd889` (PHASE-12.1 propstat parity: 404 grouping, driver-prop filtering, driver-overrides-NOT_FOUND, collection `getcontenttype`→404) and `112b968` (lowercase `D:`→`d:` namespace prefix throughout `handle_props`/`caldav`/`carddav`/`handle_lock`/`multierror`). Rebase path: fetch upstream tag, rebase the branch, bump the pin. *Pending:* push the branch to a fork and register as a git submodule of the outer repo. *(The 2026-07-31 serialization experiments — cache-control, empty-propstat guard, self-closing, xmlns dedup, encoding — were reverted uncommitted; see the 12.1 deviation.)*

### ~~2026-07-30 — 12.3/12.4 canonical root cause~~ (superseded 2026-07-31)

~~Confirmed by source trace that the home-root SHARE strip comes from `LazyUserFolder` (permissions hardcoded to `15` at the DAV bootstrap, before `setupForUser` runs), not the `sharing_mask` wrapper. Rust replicates it with an unconditional `& !16` on the mount root in `filesystem.rs::get_props`, cascading to `ocs:share-permissions`/OCM.~~ **Superseded:** the `GDNVCK`/`15` home root was a stale cold-start capture artifact; live PHP is `RGDNVCK`/`31`. The `& !16` strip was reverted and the pinning tests corrected — see the 12.3/12.4 deviations above.

### 2026-07-31 — iOS gate resolved

The iOS app (34.0.0) now lists files against Rust. Root cause and what actually changed:

- **The gate was XML namespace-prefix case, not any property value.** Rust emitted DAV-namespace props with an uppercase `D:` prefix while the rest of the response used lowercase `d:`; the iOS XML parser is case-sensitive and refused to escalate to `Depth:1`. Fixed by lowercasing the DAV prefix in `crates/nc-dav/src/props.rs` (`dc12c6a`) and throughout the vendored dav-server (`112b968`).
- **The serialization tweaks tried alongside it were reverted (handover "primary/secondary fixes" #1,2,4,6,7):** `cache-control: no-cache` removal, the empty-404-propstat guard, self-closing `<d:collection/>`, redundant-`xmlns` de-duplication, and dropping `encoding="UTF-8"`. None was causal, and the vendored crate stays generic (no SabreDAV/PHP output quirks). Only the lowercase-prefix patch and the 12.1 propstat patch are committed in the vendor repo.
- **`DAV` header `, 2` removed (`62b55fd`)** — correctness (no DAV Class 2 Locking in Rust); not causal for the gate.
- **Home-root permissions (`& !16`) reverted** — the 2026-07-30 `LazyUserFolder` analysis matched a stale cold-start capture; live PHP returns `RGDNVCK`/`31`/`["share","read","write"]` in steady state and Rust now matches (see 12.3/12.4).
- **Verification target:** the local A/B dev docker (Rust on `:8080`, php-fpm on `:9090`, same database) — not `cloud.home.lan`/`cloud2.home.lan`, which were a different/stale production environment. Remaining non-blocking divergences are queued in `phase-12-handover-2026-07-30.md` §9.

### 2026-08-03 — 12.9 owner-display-name lookup order corrected

Live A/B verification (admin display name "Wei Jian", `oc_accounts` stale at "admin") exposed that the 2026-07-29 `oc_accounts`-first resolution order diverged from PHP, which reads `oc_users.displayname` via `User::getDisplayName()` (`lib/private/User/User.php:84`) — `oc_accounts.data` is only an event-synced copy (`AccountManager.php:665-666`) and lagged. Flipped `lookup_user_display_name()` and `batch_lookup_display_names()` in `crates/nc-dav/src/row.rs` to `oc_users.displayname` → `oc_accounts.data` → UID. Verified: Rust and PHP both emit `<oc:owner-display-name>Wei Jian</oc:owner-display-name>`. `cargo test --lib -p nc-dav` → 285 passed.
