# Phase 12 — PROPFIND Response Parity

Goal: the Rust-native `PROPFIND` response for the files tree is byte-for-byte identical to PHP's where it matters to sync clients (iOS, Android, desktop). The Depth:0 root response serves as the protocol handshake that tells the client whether to proceed to Depth:1 listing. Missing or malformed fields stall the client.

Byte-for-byte parity has three structural requirements, in priority order:

1. **Emit exactly the requested properties.** PHP (SabreDAV `PropFind::handle()`) only ever fills properties present in the client's `<d:prop>` list. Rust's `build_props()` currently returns every property unconditionally.
2. **Correct 200/404 propstat grouping.** Requested properties with no value go into a second `<d:propstat>` with `<d:status>HTTP/1.1 404 Not Found</d:status>` — never as empty strings or `0` in the 200 propstat (see 12.1).
3. **Correct values** for each individual property (12.2–12.13).

## Discovery (2026-07-28 — live differential testing with iOS 34.0.0)

The iOS client would authenticate, receive a correct Depth:0 PROPFIND with root properties, but **never proceed to Depth:1** — it saw an incomplete response and considered the server incompatible. Cross-referencing the Rust response against PHP's on the same database revealed the gaps below.

**Wire captures** (kept as ground truth for this phase): Depth:0 PROPFIND pairs from `cloud.home.lan` (PHP) and `cloud2.home.lan` (Rust), 2026-07-28, same database/instance (`ocecf7uk5jlr`, root fileid 79558, iOS client 34.0.0). Where this spec cites the captures, that is the evidence.

**Environment note `[ENV]`:** the capture server runs apps that the master reference deployment does not — `text` and `files_lock` are absent from `workspace/server/` and not enabled on master (verified via `oc_appconfig`). Tasks marked `[ENV]` cannot be verified against the reference tree; for the master environment their correct behavior is a 404 propstat (subsumed by 12.1), not an empty element.

### Fixed (this session)

| Gap | Rust | PHP | Fix |
|-----|------|-----|-----|
| `DAV` response header | Missing | `1, 3, extended-mkcol, access-control, calendarserver-principal-property-search, nc-paginate, nextcloud-checksum-update, nc-calendar-search, nc-enable-birthday-calendar, 2` | Added static header matching PHP's SabreDAV output — verified character-identical against the capture |
| `d:getetag` quoting | `6a4f0c6fd3bf6` | `"6a4f0c6fd3bf6"` | `DavMetaData::etag()` (`crates/nc-dav/src/metadata.rs`) now wraps in double quotes per RFC 4918 §8.8, matching `Node::getETag()` (`apps/dav/lib/Connector/Sabre/Node.php:184-186`) |

*(The previous "oc:etag quoting" row of this table was removed: PHP does not emit `oc:etag` at all. See 12.10.)*

### Reviewed and rejected

| Old claim | Why rejected |
|-----------|--------------|
| 12.9 (old): "PHP omits `nc:creation_time` when the extended row is absent; Rust's `0` is wrong" | False. Capture shows PHP emitting `<nc:creation_time>0</nc:creation_time>` in the 200 propstat on the root. Source agrees: `FilesPlugin.php:446-448` registers it unconditionally for all nodes; `FileInfo::getCreationTime()` is a bare `(int)` cast; `Cache.php:179` normalizes a missing extended row to null → 0. Rust's `0` **matches** PHP. Implementing the old verify step would break parity. The one genuine asymmetry (directory `upload_time`) is folded into 12.1. |
| Old 12.2 mechanism: "the `R` flag appears only when the share API provider reports the file shareable; likely an is-mount-root path suppresses it" | Refuted by source — see 12.3. `DavUtil::getDavPermissions()` is pure bit logic with no shareability check. |

### Still to do

Each entry below is a concrete, scoped task, verified against the PHP reference (`workspace/server/`) and/or the wire captures. Source citations are `file:line`.

---

### 12.1 Propstat discipline and request filtering *(structural — do first)*

- [x] **PHP:** SabreDAV fills only requested properties; anything requested but left unset lands in a 404 propstat (`3rdparty/sabre/dav/lib/DAV/CorePlugin.php` `httpPropFind`; `PropFind::handle()` is a no-op for unrequested names, null return ⇒ 404). The capture's root 404 propstat contains exactly: `nc:share-download-limits`, `d:getcontenttype`, `d:getcontentlength`, `oc:checksums`, `oc:downloadURL`, `nc:upload_time`, `nc:is-encrypted`, `nc:note`, `nc:lock-owner`, `nc:lock-owner-editor`, `nc:lock-owner-displayname`, `nc:lock-owner-type`, `nc:lock-time`, `nc:lock-timeout`, `nc:file-metadata-size`, `nc:file-metadata-gps`, `nc:metadata-photos-*` (5).
- [x] **Rust gap (two parts):**
  - *(a) No 404 propstat.* The dav-server crate groups only OK-status elements; unhandled requested props are silently dropped, and `build_props()` emits empty/zero values where PHP leaves properties unset. Concrete divergences from the capture: `d:getcontenttype` on the directory (PHP 404; Rust 200 with `httpd/unix-directory`), `oc:checksums`/`oc:downloadURL`/`nc:note` (PHP 404; Rust empty 200), `nc:upload_time` on the directory (PHP 404 — `FilesPlugin.php:515-517` registers it inside the `File`-only branch; Rust 200 with `0`).
  - *(b) Request list ignored.* `build_props()` returns all properties regardless of the `<d:prop>` request. The Rust capture carries ~10 properties the client never asked for: `oc:etag`, `oc:tags`, `nc:is-mount-root`, `nc:is-federated`, `nc:hide-download`, `nc:contained-folder-count`, `nc:contained-file-count`, `nc:remind-me-at`, `nc:share-attributes`, `nc:acl-can-*`.
- [x] **Verify:** replay the captured iOS request; the Rust response must have the same 200-propstat member set and the same 404-propstat member set as PHP, with no extras. `nc:creation_time>0<` must remain in the 200 propstat (see "Reviewed and rejected").
  - *(a) dav-server vendored with patched propstat discipline (NEXTCLOUD-RS PATCH) — `core-rs/vendor/dav-server/src/handle_props.rs`. (b) build_props conditionally emits only properties with values; driver props filtered to the requested set on explicit `<prop>` requests.*

### 12.2 `d:displayname` missing

- [x] **PHP:** `FilesPlugin.php:470-472` handles `{DAV:}displayname` with `$node->getName()` (the connector `Node::getName()` → `FileInfo::getName()`, `Node.php:104-106`). For the home root that value *is* the user's UID — capture shows `<d:displayname>6c21875f5c…</d:displayname>`. (The earlier draft of this task cited `getDisplayName()` — that is wrong; no fallback logic is involved.) For files and subdirectories it is the filename.
- [x] **Rust gap:** `NcMetaData.display_name` exists and is populated from `oc_filecache.name` (`crates/nc-dav/src/metadata.rs`), and feeds `DavDirEntry::name()`, but nothing emits the `{DAV:}displayname` property. Add it to the prop set (subject to 12.1 request filtering).
- [ ] **Verify:** Depth:0 PROPFIND on the home root includes `<D:displayname>` equal to the UID; a file node returns its filename.

> **Fixed (2026-07-29):** Reordered `displayname_val` priority in `props.rs:125-136`: mount-root → UID first, then cached name, then basename fallback. Matches PHP's behavior where `View::getFileInfo()` overrides the root node's name.
>
> **Observed (2026-07-28 capture):** Rust emitted `<D:displayname>files</D:displayname>` on the root — the cached `oc_filecache.name` took priority over the mount-root → UID fallback. PHP emitted the UID.

### 12.3 `oc:permissions` — value mismatch, not letter logic

- [x] **PHP:** `DavUtil::getDavPermissions()` (`lib/public/Files/DavUtil.php:37-82`) is pure bit logic over `FileInfo::getPermissions()` (an unmasked cast of `oc_filecache.permissions`, `lib/private/Files/FileInfo.php:198-200`):
  - `S` ⇔ `FileInfo::isShared()` (mount is `ISharedMountPoint`)
  - `R` ⇔ `permissions & PERMISSION_SHARE` — **nothing else**; no shareability API, no mount-root suppression
  - `M` ⇔ `FileInfo::isMounted()` (false for home storage, `FileInfo.php:270-273`)
  - `G`/`D`/`V` ⇔ READ/DELETE/UPDATE bits
  - `N` ⇔ `DavUtil::canRename()` (`DavUtil.php:84-102`) — note its special cases: always true for the root of a movable mount; always false for the home storage's `files` directory
  - Files: `W` ⇔ writable (with a movable-mount-root check that reads the cache root entry directly, `DavUtil.php:62-70`); Directories: `CK` ⇔ CREATE bit
- [x] **Rust gap:** the letter mapping in `encode_permissions()` (`crates/nc-dav/src/props.rs`) already matches PHP's. The capture divergence (PHP `GDNVCK` vs Rust `RGDNVCK` for the *same fileid on the same database*) means the two sides feed **different permission values** into identical logic. Root cause identified: PHP's `SetupManager::setupBuiltinWrappers()` wraps storages with a `sharing_mask` `PermissionsMask(mask=15)` when sharing is disabled for the user (`ShareDisableChecker::sharingDisabledForUser()`). The mask applies at the cache layer (`CachePermissionsMask`), so every `oc_filecache` read returns permissions with the SHARE bit stripped. Rust bypassed this by reading `oc_filecache.permissions` directly. Fixed by adding `sharing_disabled_for_user()` + `apply_sharing_mask()` in `row.rs`.
- [x] **Verify:** permission strings match PHP for a matrix of nodes: home root, ordinary directory, file, received-share root, subfolder inside a share — including the `N` special cases.

> **Root cause (canonical, 2026-07-30):** the divergence is not the encoder and, on this deployment, not the `sharing_mask` wrapper. PHP builds the DAV root `Directory` from `\OC::$server->getUserFolder()` in `ServerFactory`'s `beforeMethod:*` handler — and that call runs *before* `Filesystem::getView()` triggers `setupForUser`, so `isSetupComplete` is false and `Root::getUserFolder()` returns a cached `LazyUserFolder`. `LazyUserFolder::__construct` (`lib/private/Files/Node/LazyUserFolder.php:42`) hardcodes `permissions = PERMISSION_ALL ^ PERMISSION_SHARE = 15` ("Sharing user root folder is not allowed"), and `LazyFolder::getPermissions()` returns that cached value without resolving the folder. `getDavPermissions()` → `DavUtil::getDavPermissions($this->info,…)` therefore encodes `15` → `GDNVCK`. This is unconditional and config-independent — it hits the home root even when sharing is enabled. Full trace: `phase-12-verification.md` → Resolution.
>
> **Fix (`filesystem.rs::get_props`):** after `apply_sharing_mask()`, the home root (`meta.path == "" | "files"`) strips SHARE unconditionally: `effective_permissions &= !16`. DB `31` → `15` → `GDNVCK`. The `sharing_mask` replication (`sharing_disabled_for_user()` + `apply_sharing_mask()` in `row.rs`) is retained as a secondary path — it only bites when sharing is disabled via `shareapi_exclude_groups`, which is not the capture/master case — but it is not what produces the root's `15`.
>
> **Verified:** home root (`GDNVCK` / `ocs=15` / `ocm=["read","write"]`) and ordinary directory (`RGDNVCK` / `31` / `["share","read","write"]`) match the PHP captures; pinned by 11 regression tests (`row::tests::*pipeline*`, `props::tests::permissions_dir_home_root_share_stripped`). **Deferred:** received-share root, subfolder-in-share, and the `N` special cases — no shares exist in the capture fixture.

### 12.4 `ocs:share-permissions` / OCM `share-permissions` semantics

- [x] **PHP:** `Node::getSharePermissions()` (`apps/dav/lib/Connector/Sabre/Node.php:235-276`): inside a shared storage ⇒ the share's permissions; otherwise the node's own permissions, with `DELETE|UPDATE` OR-ed in for the root of a non-moveable, non-readonly mount. Capture: `15`. `{http://open-cloud-mesh.org/ns}share-permissions` maps that through `FilesPlugin::ncPermissions2ocmPermissions()` to JSON — capture: `["read","write"]` (`FilesPlugin.php:338-348`).
- [x] **Rust gap:** `ocs:share-permissions` defaults to **31** for the owner's unshared files (MAX over `oc_share`, fallback 31 — `props.rs`, `row.rs`) instead of deriving from the node's permissions. The OCM-namespaced property is not emitted at all.
- [x] **Verify:** root emits `15` (matching PHP on the capture fixture), shared nodes emit the share's permission mask, and the OCM property appears with the JSON mapping.

> **Resolved via 12.3 (canonical, 2026-07-30):** `compute_share_permissions()` receives the SHARE-stripped permissions. At the home root that is `15` → `ocs:share-permissions=15`, and `permissions_to_ocm_json(15)` → `["read","write"]` (no SHARE bit, so `"share"` drops out). For non-root nodes the full `31` passes through → `31` / `["share","read","write"]`. Both match the PHP captures and are pinned by `row::tests::*pipeline*`. **Deferred:** the real-shared-storage case (where PHP uses the share's own mask via `Node::getSharePermissions`) — no shares in the fixture.

### 12.5 `oc:share-types` and `nc:sharees` missing

- [ ] **PHP:** `SharesPlugin` (`apps/dav/lib/Connector/Sabre/SharesPlugin.php`): `{http://owncloud.org/ns}share-types` is *always* a 200-propstat value (handler never returns null) — an empty self-closing `<oc:share-types/>` when there are none (capture confirms). Children are `<oc:share-type>int</oc:share-type>` (`ShareTypeList.php:69-73`). Shares queried are both **by** and **with** the requesting user (`getSharesBy` + `getSharedWith`, lines 87-118) across types USER(0), GROUP(1), LINK(3), EMAIL(4), REMOTE(6), CIRCLE(7), ROOM(10), DECK(12). `{http://nextcloud.org/ns}sharees` is emitted from the same plugin (lines 208-212, `ShareeList`). For Depth:1, PHP preloads per directory with one `getSharesInFolder` call (lines 166-185).
- [ ] **Rust gap:** neither property is emitted. (`share_type IN (0,1,3)` clauses in `row.rs` are unrelated query filters.)
- [ ] **Verify:** root emits empty `<oc:share-types/>` and empty `nc:sharees`; a shared file emits the correct type integers and sharee entries. Implementation note: batch the share query per listed directory for Depth:1 (constitution 2).

> **Fixed (2026-07-29):** `batch_lookup_display_names()` in `row.rs` now checks `oc_accounts.data` JSON first (via `extract_displayname_from_accounts_json()`), then `oc_users.displayname`, then the UID — same root cause as 12.9. Affects `<nc:display-name>` inside `<nc:sharee>` for user-type shares.
>
> **Observed (2026-07-28 capture):** `<oc:share-types></oc:share-types>` IS present (empty, correct). `<nc:sharees>` is absent from both 200 and 404 propstat — the capture build predates batch-2 deployment for sharees. PHP emits neither (sharees not requested by client), so the net effect is identical. Per-node queries (`get_share_details`) rather than batched per-directory (`getSharesInFolder`); batch preloading for Depth:1 deferred.

### 12.6 `oc:comments-href` / `oc:comments-count` / `oc:comments-unread` missing

- [ ] **PHP:** `CommentPropertiesPlugin` (`apps/dav/lib/Connector/Sabre/CommentPropertiesPlugin.php`) — *not* a class in the comments app. Three `{http://owncloud.org/ns}` properties, emitted for files **and** directories (line 123): `comments-count` (always an int, line 127), `comments-href` (derived from the base URI, lines 143-152), `comments-unread` (per requesting user; `null` ⇒ 404 when logged out, else integer including `0`, lines 135-137/158-166). Capture: `<oc:comments-unread>0</oc:comments-unread>`. Directory listing preloads counts in batch (`getNumberOfCommentsForObjects` / `getNumberOfUnreadCommentsForObjects`, lines 52-95).
- [ ] **Rust gap:** none of the three are emitted.
- [ ] **Verify:** root emits `comments-unread=0`, a valid `comments-href`, and `comments-count`; a commented file shows nonzero counts.

> **Observed (2026-07-28 capture):** `<oc:comments-unread>0</oc:comments-unread>` appears (correct). `<oc:comments-count>` and `<oc:comments-href>` are absent — capture build predates batch-2 deployment. Also: `comments-href` requires `overwrite.cli.url` to be set (empty → omitted by design). Per-node queries; batch helpers (`batch_comments_counts`, `batch_comments_unread`) wired for future Depth:1 optimization.

### 12.7 `nc:system-tags` missing

- [ ] **PHP:** `OCA\DAV\SystemTag\SystemTagPlugin` (`apps/dav/lib/SystemTag/SystemTagPlugin.php:54,337-346`) — *not* `\OCA\SystemTags\SabrePlugin`. Handles `{http://nextcloud.org/ns}system-tags` for file nodes; always returns a `SystemTagList` (empty ⇒ self-closing `<nc:system-tags/>`, as in the capture). Tags resolved via `ISystemTagObjectMapper::getTagIdsForObjects([fileid], 'files')` + `ISystemTagManager::getTagsByIds()`, filtered for user visibility/assignability, natural-sorted by name. `systemtags` **is** enabled on master, so this task is fully verifiable against the reference.
- [ ] **Rust gap:** not queried or emitted.
- [ ] **Verify:** empty element when no tags; inline tag objects (see `SystemTagList.php` for the exact child structure) otherwise.

> **Observed (2026-07-28 capture):** `<nc:system-tags></nc:system-tags>` matches PHP's `<nc:system-tags/>` (empty, no tags assigned). `can-assign` mirrors `user-assignable` — PHP's effective permission check (`canUserAssignTag`) not implemented. Tags filtered for `visibility=1` only.

### 12.8 `nc:mount-type` value

- [ ] **PHP:** `FilesPlugin.php:404-406` returns `MountPoint::getMountType()`: base implementation returns `''` (`lib/private/Files/Mount/MountPoint.php:268-270`) — home roots are empty, confirmed by the capture (`<nc:mount-type></nc:mount-type>`). `SharedMount` returns `'shared'` (`apps/files_sharing/lib/SharedMount.php:184-186`); `ExternalMountPoint` overrides for external storage (`apps/files_external/lib/Config/ExternalMountPoint.php:27`).
- [x] **Rust gap:** `build_props()` hardcodes `"local"` (`crates/nc-dav/src/props.rs:157`).
- [x] **Verify:** home root emits empty string; shared mounts emit `shared` when that path is implemented.

### 12.9 `oc:owner-display-name` wrong value

- [ ] **PHP:** `FilesPlugin.php:366-396`: for an authenticated request, the owner's real display name (`$owner->getDisplayName()`). Capture: `<oc:owner-display-name>Tan Siew Kin</oc:owner-display-name>`. (Public-share requests apply an account-publish-scope check and may return null ⇒ 404.)
- [x] **Rust gap:** emits the owner **UID** instead of the display name (visible in the Rust capture). Resolve the display name the same way PHP does (user accounts table), not from `oc_filecache`.
- [ ] **Verify:** root response shows the display name, not the UID.

> **Fixed (2026-07-29):** `lookup_user_display_name()` in `row.rs` now checks `oc_accounts.data` first (JSON path `displayname.value`), then falls back to `oc_users.displayname`, then the UID. This matches PHP's `$owner->getDisplayName()` → `IAccountManager` chain. `extract_displayname_from_accounts_json()` uses `serde_json` to parse the JSON blob.
>
> **Observed (2026-07-28 capture):** Rust emitted `<oc:owner-display-name>6c21875f5c...</oc:owner-display-name>` (UID). PHP emitted `<oc:owner-display-name>Tan Siew Kin</oc:owner-display-name>`. `lookup_user_display_name()` only queried `oc_users.displayname` but PHP's `$owner->getDisplayName()` checks `oc_accounts` first (IAccountManager), then falls back to `oc_users.displayname`.

### 12.10 `oc:etag` must be removed, not quoted

- [ ] **PHP:** never emits `{http://owncloud.org/ns}etag` on the files endpoint — there is no registration for it anywhere in the reference tree, and it is absent from the PHP capture's propstat.
- [x] **Rust gap:** `build_props()` emits `oc:etag` unconditionally (`crates/nc-dav/src/props.rs:118`), from the raw unquoted field (bypassing the quoting `etag()` method), and the client never requests it. The earlier "Fixed" entry treating this as a quoting gap was wrong on both counts.
- [x] **Verify:** property absent from all responses.

### 12.11 `nc:rich-workspace` missing `[ENV]`

- [ ] **Capture evidence:** PHP returns `<nc:rich-workspace></nc:rich-workspace>` (empty, 200 propstat) on the root — emitted by the Text app's rich-workspace plugin when the app is enabled.
- [ ] **Caveat:** the Text app is **not in `workspace/server/` and not enabled on master**, so the emitting source cannot be verified here. For the master environment the correct parity behavior is a 404 propstat (handled by 12.1). Implementing the empty-element behavior is only correct for deployments with the Text app, and would need that app's source to replicate (workspace file = directory `README.md` content, or empty string).
- [x] **Decision needed:** target environment. If master-only, close as subsumed by 12.1.

> **Observed (2026-07-28 capture):** PHP emits `<nc:rich-workspace></nc:rich-workspace>` in **200** propstat (Text app enabled on capture deployment). Rust emits it in **404** propstat (Text app absent from master). Decision: master-only — 404 propstat is correct for the reference environment.

### 12.12 `nc:lock` / `nc:lock-owner*` / `nc:lock-time*` missing `[ENV]`

- [ ] **Capture evidence:** PHP returns `<nc:lock></nc:lock>` in the 200 propstat and `nc:lock-owner`, `nc:lock-owner-editor`, `nc:lock-owner-displayname`, `nc:lock-owner-type`, `nc:lock-time`, `nc:lock-timeout` in the 404 propstat (unlocked file).
- [ ] **Caveat:** the attribution in the earlier draft of this task was wrong — `apps/dav/lib/Connector/Sabre/LockPlugin.php` only acquires/releases locks around PUT; it registers **no** properties, and no `nc:lock*` registration exists anywhere in `workspace/server/`. These properties come from the `files_lock` app, which is absent from the reference tree and not enabled on master. Same target-environment decision as 12.11; for master, subsumed by 12.1 (404 propstat). If implemented later against a `files_lock` checkout: `nc:lock` is always 200 (empty when unlocked), the sub-elements are 200 only while a manual lock (`oc_file_locks`) is active.
- [x] **Decision needed:** target environment.

> **Observed (2026-07-28 capture):** PHP emits `<nc:lock></nc:lock>` in **200** propstat and `nc:lock-owner*`/`nc:lock-time*` in 404 propstat (files_lock app enabled on capture deployment). Rust emits ALL lock properties in **404** propstat (files_lock absent from master). Decision: master-only — 404 propstat is correct for the reference environment.

### 12.13 `Vary: Brief,Prefer` header

- [ ] **PHP:** SabreDAV's `CorePlugin::httpPropFind` sets `Vary: Brief,Prefer` — no space after the comma — on every PROPFIND response (`3rdparty/sabre/dav/lib/DAV/CorePlugin.php:332`; also on REPORT at line 376). The earlier draft misattributed this to the BrowserPlugin and misquoted the value. Capture confirms `vary: Brief,Prefer`.
- [x] **Rust gap:** no `Vary` header anywhere in the DAV path.
- [x] **Verify:** present on all PROPFIND responses, exact value `Brief,Prefer`.

---

### Deferred findings from the 12.3 trace (2026-07-30)

Surfaced while confirming the `oc:permissions` mechanism. None is a live divergence on the home storage / master environment today; they are latent until shares, ACLs, or external mounts are served. Tracked so they are not rediscovered.

### 12.14 `N` flag: parent-CREATE fallback and movable-mount root *(latent)*

- [ ] **PHP:** `DavUtil::canRename($info, $parent)` (`lib/public/Files/DavUtil.php:84-102`) is four-way: movable-mount root (`MoveableMount` + `internalPath === ''`) ⇒ true; else updateable ⇒ true; else home storage's `files` dir ⇒ false ("can't rename the users home"); else `isDeletable() && $parent->isCreatable()`. `getDavPermissions` is passed `$this->node->getParent()` precisely for that last case.
- [ ] **Rust gap:** `props.rs:90-92` computes `can_rename = meta.permissions & 2 != 0` (the updateable case only). It never reads the parent and omits the movable-mount-root short-circuit; the code comment admits the fallback is "not checked here."
- [ ] **Impact today:** none on the home storage — every home node carries UPDATE (dirs 31 / files 27), so the updateable case always wins and the captures' `N` flags match. Diverges only for a deletable-but-not-updateable node whose parent is creatable — i.e. restricted-permission nodes (shares/ACLs).
- [ ] **Fix direction:** thread the parent's CREATE permission into `build_props` / `encode_permissions` and add the movable-mount-root case. Fidelity hardening, not a behavior fix, until shares/ACLs exist.

### 12.15 Home-root permissions: hardcode vs derive *(latent)*

- [ ] **PHP:** `LazyUserFolder` hardcodes `permissions = 15` and `getPermissions()` returns it *without resolving* the folder (`lib/private/Files/Node/LazyUserFolder.php:42`, `LazyFolder::getPermissions`). The home root reports 15 regardless of `oc_filecache.permissions`.
- [ ] **Rust gap:** `filesystem.rs::get_props` derives `apply_sharing_mask(db_perms) & !16`. For a normal home root (`db_perms = 31`) this equals 15, but the semantic differs — PHP *hardcodes* 15, Rust *derives* it. If a home root's `oc_filecache.permissions` were ever ≠ 31, the two would diverge.
- [ ] **Decision needed:** hardcode 15 at the mount root to mirror PHP exactly, or keep the derive and document the assumption that home roots are always `PERMISSION_ALL`. Low priority — home roots are `PERMISSION_ALL` in practice.

### 12.16 Share / external-mount permission paths *(deferred until native shares)*

Correct for the home storage today; will diverge once received shares / external mounts are served. Grouped because they share the "not a home mount" trigger:

- [ ] `S` / `M` flags: Rust hardcodes `is_shared = false` and derives `is_mounted` from the storage `string_id`. PHP uses `FileInfo::isShared()` (`ISharedMountPoint`) and `isMounted()`.
- [ ] `Node::getSharePermissions()` (`apps/dav/lib/Connector/Sabre/Node.php:235-276`): the federated-token short-circuit (`getShareByToken`), the `ISharedStorage` ⇒ share-mask branch, and the `MoveableMount` DELETE|UPDATE injection for non-readonly mount roots are not replicated (Rust uses the home-only `compute_share_permissions`).
- [ ] `W` movable-mount-root re-derivation (`DavUtil.php:62-70`, documented deviation improvements §I.9): PHP re-reads the storage root cache entry for a file mounted at its own root; single-file shares only.
- [ ] `View::getFileInfo` `MoveableMount`-root DELETE injection (`$data['permissions'] |= PERMISSION_DELETE` when `internalPath === ''`, `lib/private/Files/View.php`): affects share roots; home is not a `MoveableMount`.

> **Not actionable (recorded for completeness):** `nc:rich-workspace` / `nc:lock` 200-vs-404 placement is `[ENV]` — correct for master (see 12.11/12.12), diverges only from capture deployments running `text` / `files_lock`. The `resourcetype` element serializes as `<D:collection></D:collection>` (Rust) vs `<d:collection/>` (PHP) — same `{DAV:}collection` value, identical to any XML parser; cosmetic only.

---

## Changes

### dav-server vendored (2026-07-29)

`core-rs/vendor/dav-server` — clone of `messense/dav-server-rs`, branch `nextcloud-0.11.0` based on tag `v0.11.0` (verified byte-identical to the crates.io tarball), patch commit `b9cd889`, wired via `[patch.crates-io]`. Patches (all marked `NEXTCLOUD-RS PATCH`): requested-but-unavailable properties grouped into a 404 propstat instead of dropped; driver properties filtered to the requested set on explicit `<prop>` requests (allprop/propname unchanged); driver props override NOT_FOUND placeholders for the same name; `getcontenttype` 404 for collections. Rebase path: fetch upstream tag, rebase the branch, bump the pin. *Pending:* push the branch to a fork and register as a git submodule of the outer repo.

### 2026-07-30 — 12.3/12.4 canonical root cause

Confirmed by source trace that the home-root SHARE strip comes from `LazyUserFolder` (permissions hardcoded to `15` at the DAV bootstrap, before `setupForUser` runs), not the `sharing_mask` wrapper. Rust replicates it with an unconditional `& !16` on the mount root in `filesystem.rs::get_props`, cascading to `ocs:share-permissions`/OCM. See the 12.3/12.4 deviation notes above and `phase-12-verification.md` → Resolution. Pinned by 11 regression tests; `cargo test --lib -p nc-dav` → 284 passed, `cargo check --workspace` clean. End-to-end recapture against the rebuilt binary still outstanding.
