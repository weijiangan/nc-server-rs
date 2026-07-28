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

- [ ] **PHP:** SabreDAV fills only requested properties; anything requested but left unset lands in a 404 propstat (`3rdparty/sabre/dav/lib/DAV/CorePlugin.php` `httpPropFind`; `PropFind::handle()` is a no-op for unrequested names, null return ⇒ 404). The capture's root 404 propstat contains exactly: `nc:share-download-limits`, `d:getcontenttype`, `d:getcontentlength`, `oc:checksums`, `oc:downloadURL`, `nc:upload_time`, `nc:is-encrypted`, `nc:note`, `nc:lock-owner`, `nc:lock-owner-editor`, `nc:lock-owner-displayname`, `nc:lock-owner-type`, `nc:lock-time`, `nc:lock-timeout`, `nc:file-metadata-size`, `nc:file-metadata-gps`, `nc:metadata-photos-*` (5).
- [ ] **Rust gap (two parts):**
  - *(a) No 404 propstat.* The dav-server crate groups only OK-status elements; unhandled requested props are silently dropped, and `build_props()` emits empty/zero values where PHP leaves properties unset. Concrete divergences from the capture: `d:getcontenttype` on the directory (PHP 404; Rust 200 with `httpd/unix-directory`), `oc:checksums`/`oc:downloadURL`/`nc:note` (PHP 404; Rust empty 200), `nc:upload_time` on the directory (PHP 404 — `FilesPlugin.php:515-517` registers it inside the `File`-only branch; Rust 200 with `0`).
  - *(b) Request list ignored.* `build_props()` returns all properties regardless of the `<d:prop>` request. The Rust capture carries ~10 properties the client never asked for: `oc:etag`, `oc:tags`, `nc:is-mount-root`, `nc:is-federated`, `nc:hide-download`, `nc:contained-folder-count`, `nc:contained-file-count`, `nc:remind-me-at`, `nc:share-attributes`, `nc:acl-can-*`.
- [ ] **Verify:** replay the captured iOS request; the Rust response must have the same 200-propstat member set and the same 404-propstat member set as PHP, with no extras. `nc:creation_time>0<` must remain in the 200 propstat (see "Reviewed and rejected").

### 12.2 `d:displayname` missing

- [ ] **PHP:** `FilesPlugin.php:470-472` handles `{DAV:}displayname` with `$node->getName()` (the connector `Node::getName()` → `FileInfo::getName()`, `Node.php:104-106`). For the home root that value *is* the user's UID — capture shows `<d:displayname>6c21875f5c…</d:displayname>`. (The earlier draft of this task cited `getDisplayName()` — that is wrong; no fallback logic is involved.) For files and subdirectories it is the filename.
- [ ] **Rust gap:** `NcMetaData.display_name` exists and is populated from `oc_filecache.name` (`crates/nc-dav/src/metadata.rs`), and feeds `DavDirEntry::name()`, but nothing emits the `{DAV:}displayname` property. Add it to the prop set (subject to 12.1 request filtering).
- [ ] **Verify:** Depth:0 PROPFIND on the home root includes `<D:displayname>` equal to the UID; a file node returns its filename.

### 12.3 `oc:permissions` — value mismatch, not letter logic

- [ ] **PHP:** `DavUtil::getDavPermissions()` (`lib/public/Files/DavUtil.php:37-82`) is pure bit logic over `FileInfo::getPermissions()` (an unmasked cast of `oc_filecache.permissions`, `lib/private/Files/FileInfo.php:198-200`):
  - `S` ⇔ `FileInfo::isShared()` (mount is `ISharedMountPoint`)
  - `R` ⇔ `permissions & PERMISSION_SHARE` — **nothing else**; no shareability API, no mount-root suppression
  - `M` ⇔ `FileInfo::isMounted()` (false for home storage, `FileInfo.php:270-273`)
  - `G`/`D`/`V` ⇔ READ/DELETE/UPDATE bits
  - `N` ⇔ `DavUtil::canRename()` (`DavUtil.php:84-102`) — note its special cases: always true for the root of a movable mount; always false for the home storage's `files` directory
  - Files: `W` ⇔ writable (with a movable-mount-root check that reads the cache root entry directly, `DavUtil.php:62-70`); Directories: `CK` ⇔ CREATE bit
- [ ] **Rust gap:** the letter mapping in `encode_permissions()` (`crates/nc-dav/src/props.rs`) already matches PHP's. The capture divergence (PHP `GDNVCK` vs Rust `RGDNVCK` for the *same fileid on the same database*) means the two sides feed **different permission values** into identical logic: PHP's value for the root lacks the SHARE bit, Rust's includes it (Rust's `ocs:share-permissions`=31 vs PHP's 15 corroborates). Rust reads `oc_filecache.permissions` directly (`crates/nc-dav/src/row.rs`), so the divergence must be traced against the capture database — note it is **not** `master-database-pgsql-1` (fileid 79558 does not exist there). Do **not** implement `R` suppression: it would produce the right string on this fixture for the wrong reason and strip `R` from roots whose cache row genuinely has the SHARE bit.
- [ ] **Verify:** permission strings match PHP for a matrix of nodes: home root, ordinary directory, file, received-share root, subfolder inside a share — including the `N` special cases.

### 12.4 `ocs:share-permissions` / OCM `share-permissions` semantics

- [ ] **PHP:** `Node::getSharePermissions()` (`apps/dav/lib/Connector/Sabre/Node.php:235-276`): inside a shared storage ⇒ the share's permissions; otherwise the node's own permissions, with `DELETE|UPDATE` OR-ed in for the root of a non-moveable, non-readonly mount. Capture: `15`. `{http://open-cloud-mesh.org/ns}share-permissions` maps that through `FilesPlugin::ncPermissions2ocmPermissions()` to JSON — capture: `["read","write"]` (`FilesPlugin.php:338-348`).
- [ ] **Rust gap:** `ocs:share-permissions` defaults to **31** for the owner's unshared files (MAX over `oc_share`, fallback 31 — `props.rs`, `row.rs`) instead of deriving from the node's permissions. The OCM-namespaced property is not emitted at all.
- [ ] **Verify:** root emits `15` (matching PHP on the capture fixture), shared nodes emit the share's permission mask, and the OCM property appears with the JSON mapping.

### 12.5 `oc:share-types` and `nc:sharees` missing

- [ ] **PHP:** `SharesPlugin` (`apps/dav/lib/Connector/Sabre/SharesPlugin.php`): `{http://owncloud.org/ns}share-types` is *always* a 200-propstat value (handler never returns null) — an empty self-closing `<oc:share-types/>` when there are none (capture confirms). Children are `<oc:share-type>int</oc:share-type>` (`ShareTypeList.php:69-73`). Shares queried are both **by** and **with** the requesting user (`getSharesBy` + `getSharedWith`, lines 87-118) across types USER(0), GROUP(1), LINK(3), EMAIL(4), REMOTE(6), CIRCLE(7), ROOM(10), DECK(12). `{http://nextcloud.org/ns}sharees` is emitted from the same plugin (lines 208-212, `ShareeList`). For Depth:1, PHP preloads per directory with one `getSharesInFolder` call (lines 166-185).
- [ ] **Rust gap:** neither property is emitted. (`share_type IN (0,1,3)` clauses in `row.rs` are unrelated query filters.)
- [ ] **Verify:** root emits empty `<oc:share-types/>` and empty `nc:sharees`; a shared file emits the correct type integers and sharee entries. Implementation note: batch the share query per listed directory for Depth:1 (constitution 2).

### 12.6 `oc:comments-href` / `oc:comments-count` / `oc:comments-unread` missing

- [ ] **PHP:** `CommentPropertiesPlugin` (`apps/dav/lib/Connector/Sabre/CommentPropertiesPlugin.php`) — *not* a class in the comments app. Three `{http://owncloud.org/ns}` properties, emitted for files **and** directories (line 123): `comments-count` (always an int, line 127), `comments-href` (derived from the base URI, lines 143-152), `comments-unread` (per requesting user; `null` ⇒ 404 when logged out, else integer including `0`, lines 135-137/158-166). Capture: `<oc:comments-unread>0</oc:comments-unread>`. Directory listing preloads counts in batch (`getNumberOfCommentsForObjects` / `getNumberOfUnreadCommentsForObjects`, lines 52-95).
- [ ] **Rust gap:** none of the three are emitted.
- [ ] **Verify:** root emits `comments-unread=0`, a valid `comments-href`, and `comments-count`; a commented file shows nonzero counts.

### 12.7 `nc:system-tags` missing

- [ ] **PHP:** `OCA\DAV\SystemTag\SystemTagPlugin` (`apps/dav/lib/SystemTag/SystemTagPlugin.php:54,337-346`) — *not* `\OCA\SystemTags\SabrePlugin`. Handles `{http://nextcloud.org/ns}system-tags` for file nodes; always returns a `SystemTagList` (empty ⇒ self-closing `<nc:system-tags/>`, as in the capture). Tags resolved via `ISystemTagObjectMapper::getTagIdsForObjects([fileid], 'files')` + `ISystemTagManager::getTagsByIds()`, filtered for user visibility/assignability, natural-sorted by name. `systemtags` **is** enabled on master, so this task is fully verifiable against the reference.
- [ ] **Rust gap:** not queried or emitted.
- [ ] **Verify:** empty element when no tags; inline tag objects (see `SystemTagList.php` for the exact child structure) otherwise.

### 12.8 `nc:mount-type` value

- [ ] **PHP:** `FilesPlugin.php:404-406` returns `MountPoint::getMountType()`: base implementation returns `''` (`lib/private/Files/Mount/MountPoint.php:268-270`) — home roots are empty, confirmed by the capture (`<nc:mount-type></nc:mount-type>`). `SharedMount` returns `'shared'` (`apps/files_sharing/lib/SharedMount.php:184-186`); `ExternalMountPoint` overrides for external storage (`apps/files_external/lib/Config/ExternalMountPoint.php:27`).
- [ ] **Rust gap:** `build_props()` hardcodes `"local"` (`crates/nc-dav/src/props.rs:157`).
- [ ] **Verify:** home root emits empty string; shared mounts emit `shared` when that path is implemented.

### 12.9 `oc:owner-display-name` wrong value

- [ ] **PHP:** `FilesPlugin.php:366-396`: for an authenticated request, the owner's real display name (`$owner->getDisplayName()`). Capture: `<oc:owner-display-name>Tan Siew Kin</oc:owner-display-name>`. (Public-share requests apply an account-publish-scope check and may return null ⇒ 404.)
- [ ] **Rust gap:** emits the owner **UID** instead of the display name (visible in the Rust capture). Resolve the display name the same way PHP does (user accounts table), not from `oc_filecache`.
- [ ] **Verify:** root response shows the display name, not the UID.

### 12.10 `oc:etag` must be removed, not quoted

- [ ] **PHP:** never emits `{http://owncloud.org/ns}etag` on the files endpoint — there is no registration for it anywhere in the reference tree, and it is absent from the PHP capture's propstat.
- [ ] **Rust gap:** `build_props()` emits `oc:etag` unconditionally (`crates/nc-dav/src/props.rs:118`), from the raw unquoted field (bypassing the quoting `etag()` method), and the client never requests it. The earlier "Fixed" entry treating this as a quoting gap was wrong on both counts.
- [ ] **Verify:** property absent from all responses.

### 12.11 `nc:rich-workspace` missing `[ENV]`

- [ ] **Capture evidence:** PHP returns `<nc:rich-workspace></nc:rich-workspace>` (empty, 200 propstat) on the root — emitted by the Text app's rich-workspace plugin when the app is enabled.
- [ ] **Caveat:** the Text app is **not in `workspace/server/` and not enabled on master**, so the emitting source cannot be verified here. For the master environment the correct parity behavior is a 404 propstat (handled by 12.1). Implementing the empty-element behavior is only correct for deployments with the Text app, and would need that app's source to replicate (workspace file = directory `README.md` content, or empty string).
- [ ] **Decision needed:** target environment. If master-only, close as subsumed by 12.1.

### 12.12 `nc:lock` / `nc:lock-owner*` / `nc:lock-time*` missing `[ENV]`

- [ ] **Capture evidence:** PHP returns `<nc:lock></nc:lock>` in the 200 propstat and `nc:lock-owner`, `nc:lock-owner-editor`, `nc:lock-owner-displayname`, `nc:lock-owner-type`, `nc:lock-time`, `nc:lock-timeout` in the 404 propstat (unlocked file).
- [ ] **Caveat:** the attribution in the earlier draft of this task was wrong — `apps/dav/lib/Connector/Sabre/LockPlugin.php` only acquires/releases locks around PUT; it registers **no** properties, and no `nc:lock*` registration exists anywhere in `workspace/server/`. These properties come from the `files_lock` app, which is absent from the reference tree and not enabled on master. Same target-environment decision as 12.11; for master, subsumed by 12.1 (404 propstat). If implemented later against a `files_lock` checkout: `nc:lock` is always 200 (empty when unlocked), the sub-elements are 200 only while a manual lock (`oc_file_locks`) is active.
- [ ] **Decision needed:** target environment.

### 12.13 `Vary: Brief,Prefer` header

- [ ] **PHP:** SabreDAV's `CorePlugin::httpPropFind` sets `Vary: Brief,Prefer` — no space after the comma — on every PROPFIND response (`3rdparty/sabre/dav/lib/DAV/CorePlugin.php:332`; also on REPORT at line 376). The earlier draft misattributed this to the BrowserPlugin and misquoted the value. Capture confirms `vary: Brief,Prefer`.
- [ ] **Rust gap:** no `Vary` header anywhere in the DAV path.
- [ ] **Verify:** present on all PROPFIND responses, exact value `Brief,Prefer`.

---

## Changes

### 2026-07-29 — implementation, batch 1

Implemented **12.2, 12.8, 12.10, 12.13** and both code halves of **12.1** (framework patch + value discipline). `cargo test --lib` green across the workspace.

- **dav-server vendored** at `core-rs/vendor/dav-server` — clone of `messense/dav-server-rs`, branch `nextcloud-0.11.0` based on tag `v0.11.0` (verified byte-identical to the crates.io tarball), patch commit `b9cd889`, wired via `[patch.crates-io]`. Patches (all marked `NEXTCLOUD-RS PATCH`): requested-but-unavailable properties grouped into a 404 propstat instead of dropped; driver properties filtered to the requested set on explicit `<prop>` requests (allprop/propname unchanged); driver props override NOT_FOUND placeholders for the same name; `getcontenttype` 404 for collections. Rebase path: fetch upstream tag, rebase the branch, bump the pin. *Pending:* push the branch to a fork and register as a git submodule of the outer repo.
- **12.1 value discipline** (`crates/nc-dav/src/props.rs`): `checksums`/`downloadURL`/`note` omitted when empty, `upload_time` omitted for directories, `hide-download` only on shared nodes, `share-attributes` now `[]` (PHP's `json_encode`); removed `acl-can-*` and `remind-me-at` (not PHP-core; PHP 404s them).
- **12.2:** `{DAV:}displayname` emitted — cached name, UID at mount roots (mirrors `FileInfo::getName()`).
- **12.8:** `mount-type` now `""` (home) / `"shared"` (shared) — was hardcoded `"local"`.
- **12.10:** `oc:etag` no longer emitted.
- **12.13:** `Vary: Brief,Prefer` on PROPFIND/REPORT.

**Verification status:** unit-tested behavior; the end-to-end check — replaying the captured iOS request against a deployed Rust build and diffing 200/404 propstat sets against PHP — is still pending.

### 2026-07-29 — spec accuracy review

Reviewed every claim against the PHP reference source and the 2026-07-28 wire captures; rewrote accordingly:

- Reframed the phase around request filtering + propstat discipline (new 12.1) as the structural prerequisite.
- 12.3 (old 12.2): refuted the shareability/`is-mount-root` hypothesis against `DavUtil` source; re-targeted the task at the actual bug (Rust's permission *value* diverges from `oc_filecache.permissions`).
- 12.10 (old "Fixed" oc:etag row): PHP never emits `oc:etag`; task is removal, not quoting.
- Old 12.9 (creation_time omission): rejected — PHP emits `0` in the 200 propstat; directory `upload_time` folded into 12.1.
- 12.12 (old 12.6): `LockPlugin` emits no properties; `nc:lock*` originates in `files_lock`, absent from the reference — marked `[ENV]`; same for rich-workspace (12.11).
- Fixed class attributions (CommentPropertiesPlugin, SystemTagPlugin, CorePlugin) and mechanism claims (displayname via `getName()`, `Vary` value `Brief,Prefer`).
- Added gaps the capture shows but the original list missed: owner-display-name (12.9), share-permissions semantics + OCM property (12.4), `nc:sharees` (12.5).

### 2026-07-28 — `d:getetag` quoting and `DAV` header

- Quoted ETag in the `DavMetaData::etag()` impl per RFC 4918 §8.8.
- Added static `DAV` response header matching PHP's SabreDAV plugin set (verified character-identical against the capture).
