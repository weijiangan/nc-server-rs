# Phase 4 — DAV Files Tree and Properties

Goal: PROPFIND on a user file tree returns the correct properties; all RFC 4918 methods work; DAV compliance passes litmus.

---

### 4.1 `DavFileSystem` — `read_dir`
- [x] `SELECT fileid, name, mimetype, size, mtime, etag, permissions, checksum FROM oc_filecache WHERE parent = ? AND storage = ?`
- [x] Returns `DavDirEntry` for each child node
- [x] Uses mime-type cache (Phase 0.5) — no join to `oc_mimetypes` per row
- [x] Load `oc_filecache_extended` for all children in a single batch query so `{nc:}creation_time`, `{nc:}upload_time`, and `{nc:}metadata_etag` are correct on depth-1 PROPFIND

**Verify:** PROPFIND depth-1 on a directory with 10 children returns 11 responses (1 collection + 10 children). Assert no `oc_mimetypes` query executed. Assert `{nc:}creation_time` on each child matches `oc_filecache_extended.creation_time`.

### 4.2 `DavFileSystem` — `metadata`
- [x] Resolve path → `oc_filecache` row via `path_hash = md5(path)` lookup
- [x] Returns `DavMetaData` with size, mtime, is_dir, etag
- [x] `oc_filecache_extended` loaded and applied (`creation_time`, `upload_time`, `metadata_etag`)
- [x] `content_type()` method on `NcMetaData` returns stored MIME type for `{DAV:}getcontenttype`

**Verify:** PROPFIND depth-0 on a known file returns correct size, mtime, and `{DAV:}getcontenttype` matching the value in `oc_mimetypes`.

### 4.3 `DavFileSystem` — `get_file`
- [x] Resolve `oc_filecache` row → open file from `datadirectory/{storageId}/{path}`
- [x] Stream response body; no full-file buffering in memory
- [x] `X-Accel-Buffering: no` on response
- [x] `Content-Disposition: attachment; filename*=UTF-8''…` forced on all file downloads (matches PHP `FilesPlugin::httpGet`)
- [x] Include `OC-Checksum` header if `oc_filecache.checksum` is non-empty

**Verify:** `build/integration/files_features/download.feature` and `build/integration/files_features/checksums.feature` — GET download assertions pass. Confirm `Content-Disposition: attachment` on `.mp4` download; absent on `text/plain`.

### 4.4 `DavFileSystem` — `put_file`
- [x] Write stream to `datadirectory/{storageId}/{path}` atomically (write to temp, rename)
- [x] Update `oc_filecache`: `size`, `mtime`, `storage_mtime`, `etag` (new md5), `checksum`, `upload_time`
- [x] UPSERT `oc_filecache_extended` on every PUT: `upload_time = use_mtime`, preserve existing `creation_time` on UPDATE
- [x] Response headers: `OC-FileId`, `ETag`, `OC-ETag`, `X-OC-MTime: accepted` if mtime honored
- [x] `201 Created` for new file, `204 No Content` for overwrite
- [x] `OC-Checksum` validated against computed hash (MD5, SHA1, SHA256); mismatch deletes temp and rejects write
- [x] Checksum mismatch returns `400 Bad Request` (currently bubbles up as 500 via `FsError::GeneralFailure`)
- [x] Adler32 (`ADLER32:`) supported as a checksum algorithm alongside MD5/SHA1/SHA256

**Verify:** PUT a file, then PROPFIND — assert `{oc:}fileid` and `{DAV:}getetag` match the values in the response headers from the PUT. PUT with wrong `OC-Checksum` → `400`. PUT with `OC-Checksum: ADLER32:…` → validated correctly.

### 4.5 `DavFileSystem` — `remove_file`, `remove_dir`, `create_dir`
- [x] `remove_file`: delete from storage + `DELETE FROM oc_filecache WHERE fileid = ?`
- [x] `remove_file`: also `DELETE FROM oc_filecache_extended WHERE fileid = ?`
- [x] `remove_dir`: recursive delete from storage + `oc_filecache` subtree (LIKE-prefix delete)
- [x] `remove_dir`: also delete orphaned rows in `oc_filecache_extended` for the removed fileids
- [x] `create_dir`: `INSERT INTO oc_filecache` for the new collection node; create directory on disk

**Verify:** `build/integration/dav_features/webdav-related.feature` — DELETE and MKCOL scenarios. After DELETE, confirm no orphan row in `oc_filecache_extended`.

### 4.6 Path resolution — `/remote.php/webdav` alias
- [x] `GET /remote.php/webdav/{path}` resolves to `home::{userId}` storage, root `files/`
- [x] Same as `/dav/files/{userId}/{path}`
- [x] `/remote.php/dav/files/{userId}/{path}` strips prefix correctly (currently produces a double `files/` component, resolving to a non-existent path)

**Verify:** `build/integration/dav_features/webdav-related.feature` — v1 and v2 root path resolution both succeed.

### 4.7 Standard DAV properties
- [x] `{DAV:}getetag`, `{DAV:}getlastmodified`, `{DAV:}getcontentlength`, `{DAV:}resourcetype`, `{DAV:}creationdate`, `{DAV:}displayname` returned correctly
- [x] `{DAV:}getcontenttype` reflects stored MIME type from cache (dav-server uses `mime_guess::from_ext()` from the URL path, which matches what Nextcloud stores at upload time — same crate and logic; `content_type()` on `NcMetaData` is used for GET `Content-Type` headers via handler.rs override)
- [x] `{DAV:}getetag` writable via PROPPATCH; `{DAV:}displayname` returns `403` on PROPPATCH
- [x] `{DAV:}quota-available-bytes` returns `-3` for unlimited quota; injected via `build_props()` since `get_quota()` returns `None` for total (unlimited) so dav-server suppresses its own emit
- [x] `{DAV:}quota-used-bytes` = sum of user's `oc_filecache.size`; `get_quota()` now queries the `files/` root entry whose `size` column is maintained by Nextcloud as the recursive total

**Verify:** PROPFIND with `allprop` on a file; assert all standard properties present. PROPPATCH `{DAV:}displayname` → `403`. Quota props return non-zero used bytes.

### 4.8 OC namespace properties (`{oc:}`)
- [x] `{oc:}fileid`, `{oc:}id`, `{oc:}etag`, `{oc:}size`, `{oc:}owner-id`, `{oc:}checksum`, `{oc:}data-fingerprint`, `{oc:}has-preview`, `{oc:}checksums`, `{oc:}contained-folder-count`, `{oc:}contained-file-count` present
- [x] `{oc:}id` = `fileid` zero-padded to 8 chars + instance ID
- [x] `{oc:}permissions` encoded as string (`R`, `W`, `CK`, `D`, `S`); correct deduplication of `CK`
- [x] `{oc:}owner-display-name` returns the user's display name from `oc_users.displayname` (currently returns raw UID)
- [x] `{oc:}downloadURL` property present (currently missing; return empty string as placeholder — full URL generation requires router context, deferred to Phase 7)
- [x] `{oc:}share-permissions` integer bitmask present (default `31` for owner; full per-share logic deferred to Phase 7)
- [ ] `M` (mounted) flag in `{oc:}permissions` string — deferred to Phase 7 (no mount storage types yet)

**Verify:** PROPFIND response contains `{oc:}fileid`, `{oc:}permissions`, `{oc:}size`, `{oc:}owner-id`, `{oc:}owner-display-name`, `{oc:}share-permissions` with correct values cross-checked against DB.

### 4.9 NC namespace properties (`{nc:}`)
- [x] `{nc:}creation_time`, `{nc:}upload_time` read from `oc_filecache_extended` (authoritative source) for `metadata()` calls
- [x] `{nc:}metadata_etag` from `oc_filecache_extended.metadata_etag` (deliberately wired — absent in PHP reference implementation)
- [x] `{nc:}mount-type`, `{nc:}is-federated`, `{nc:}contained-folder-count`, `{nc:}contained-file-count` present
- [x] `{nc:}is-mount-root` — `"true"` when `meta.path` is `"files"` or `""`; `"false"` otherwise
- [x] `{nc:}hide-download` — present with value `"false"` for home tree nodes
- [ ] `{nc:}metadata-{key}` — dynamic per-file metadata properties (in scope: inline on the Rust-native PROPFIND, cannot be proxied). Emit one `{nc:}metadata-{key}` for **every** key present in the node's metadata: PHP iterates `FileInfo::getMetadata()` and calls `$propFind->handle(FILE_METADATA_PREFIX . $key, $value)` (`apps/dav/lib/Connector/Sabre/FilesPlugin.php:444-446`; prefix `{http://nextcloud.org/ns}metadata-` at `:74`). Source = `oc_files_metadata` (JSON `metadata` column + indexed values) via the files-metadata manager. Not a fixed list — whatever keys exist for the node (e.g. `photos-size`, `files-live-photo`, `gps`)
- [ ] `{nc:}download-url-expiration` — **deferred to Phase 6** (public-share feature)
- [ ] `{nc:}hidden` — **deferred to Phase 6** (live-photo MOV companion detection)
- [ ] `{nc:}note` hard-coded empty string; share note from `oc_share.note` — **deferred to Phase 7**

**Verify:** `build/integration/files_features/metadata.feature`. Confirm `{nc:}metadata_etag` present in PROPFIND response. Confirm `{nc:}is-mount-root` is `"true"` on the user's `files/` root collection.

### 4.10 PROPPATCH writable properties
- [x] `{DAV:}lastmodified` → update `mtime` in `oc_filecache`
- [x] `{DAV:}getetag` → set custom ETag
- [x] `{DAV:}creationdate` / `{nc:}creation_time` → update `oc_filecache_extended.creation_time`
- [x] `{DAV:}displayname` → `403 Forbidden`
- [x] `creationdate` PROPPATCH UPSERT preserves existing `upload_time` from `oc_filecache` for new extended rows (same for `{nc:}creation_time` and `{nc:}upload_time`)
- [ ] `{nc:}metadata-{key}` PROPPATCH → write `oc_files_metadata` (in scope: inline on the Rust-native PROPPATCH). Per `handleUpdatePropertiesMetadata` (`apps/dav/lib/Connector/Sabre/FilesPlugin.php:623`): for each mutation prefixed `{http://nextcloud.org/ns}metadata-`, **permission-check per key** — reject when the key's edit-permission requirement exceeds the user's access right to the node (`$knownMetadata->getEditPermission($key) < $accessRight`, `:~645`). A `null` value **unsets** the key; otherwise set by the key's declared type (string/int/float/bool/array/string-list/int-list), flagging indexed keys, then persist via the files-metadata manager

**Verify:** PROPPATCH each writable property; subsequent PROPFIND returns the new value. Confirm `{nc:}upload_time` unchanged after a `creationdate` PROPPATCH.

### 4.12 `SEARCH` method (DAV basic search)
> PHP source: the `SearchDAV` library's `SearchPlugin`, registered at `apps/dav/lib/Server.php:281` (handles the `SEARCH` method and the `{DAV:}searchrequest` → `{DAV:}basicsearch` grammar, returning `207 Multi-Status`); the Nextcloud backend `apps/dav/lib/Files/FileSearchBackend.php` defines the searchable schema and translates the query.

- [ ] `SEARCH` is handled at the DAV **arbiter root** — `getArbiterPath()` returns `''` → `/remote.php/dav/` (and `/dav/`), **not** `/dav/files/{userId}` (`FileSearchBackend.php:62`). The `{DAV:}basicsearch` body's `{DAV:}from`/`scope` entries carry a `path` relative to the arbiter (e.g. `/files/{userId}/Photos`); each scope must resolve to a **directory** or the request is rejected (`isValidScope` `FileSearchBackend.php:66`, `getFolderForPath` throws otherwise)
- [ ] Queryable properties (usable in `WHERE`, from `getPropertyDefinitionsForScope` `FileSearchBackend.php:80-104`): `{DAV:}displayname`, `{DAV:}getcontenttype`, `{DAV:}getlastmodified` (datetime), `{DAV:}creationdate` (datetime), `{nc:}upload_time` (datetime), `{oc:}size` (non-negative int), `{oc:}favorite` (boolean), `{oc:}fileid` (`INTERNAL_FILEID_PROPERTYNAME`, non-negative int), `{oc:}owner-id`, plus dynamic `{nc:}metadata-{key}` for **indexed** metadata keys
- [ ] Select-only properties (returnable, not searchable): `{DAV:}resourcetype`, `{DAV:}getcontentlength`, `{oc:}checksums`, `{oc:}permissions`, `{DAV:}getetag`, `{oc:}owner-display-name`, `{oc:}data-fingerprint`, `{nc:}has-preview`, `{oc:}id` (`FILEID_PROPERTYNAME`)
- [ ] Operators (`transformSearchOperation` `FileSearchBackend.php:~283`): `eq`, `lt`, `lte`, `gt`, `gte`, `like` (contains), and boolean `and`/`or`/`not`. Exceeding `OPERATOR_LIMIT = 100` operators (`FileSearchBackend.php:46`) → reject with `400`/InvalidArgument
- [ ] `{oc:}owner-id` filter must equal the current user (sets limit-to-home); any other value is rejected with InvalidArgument (`transformQuery` `FileSearchBackend.php:~340`)
- [ ] Paging/sort: `{DAV:}limit` `maxResults` (`0` = unlimited, `array_slice` at `:233`) + `firstResult` offset; `orderby` supported, including metadata keys
- [ ] Result `href` = `/files/{userId}/{relativePath}` (`getHrefForNode` `FileSearchBackend.php:~318`)
- [ ] Response: `207 Multi-Status`, one `<d:response>` per match carrying the requested `{DAV:}prop` set

**Verify:** `build/integration/dav_features/webdav-related.feature` — SEARCH scenarios; or integration test: `SEARCH /remote.php/dav` with a `{DAV:}basicsearch` body scoped to `/files/{userId}` filtering `{DAV:}displayname` `like` — assert only matching files return, a non-directory scope → error, and `>100` operators → `400`.

### 4.13 `DavLockSystem` — fake locker
- [x] `LOCK` → `200 OK` with `lockdiscovery` body (via `FakeLs`)
- [x] `UNLOCK` → `204 No Content`
- [x] PROPFIND `{DAV:}lockdiscovery` → empty lock list
- [x] PROPFIND `{DAV:}supportedlock` → shared + exclusive
- [x] Lock token derived from `md5(path)` as `urn:uuid:{md5_hex(path)}` — replace `FakeLs` with custom `NcLockSystem` implementing `DavLockSystem`
- [x] `If:` header lock tokens always validated as valid (stateless — `check()` returns `Ok(None)` always)

**Verify:** macOS WebDAV mount; LOCK/UNLOCK round trip returns correct status codes. `litmus` lock tests pass.

### 4.14 DAV compatibility plugins
- 4.14.1 [x] `Content-Security-Policy: default-src 'none';` set on every response from `/remote.php`, `/public.php`, and `/dav` endpoints (REQ §2.4)
- 4.14.2 [x] `CopyEtagHeaderPlugin`: `OC-ETag` mirrors `ETag` on every response that has an ETag
- 4.14.3 [ ] `AnonymousOptionsPlugin`: unauthenticated OPTIONS/HEAD from MS Office UA (`Microsoft-WebDAV`, `Microsoft Office`) → `200` with DAV headers, no auth prompt
- 4.14.4 [ ] `AppleQuirksPlugin`: no-op (replicate the PHP bug — UA check uses `macOS/` which never matches; document as intentional)
- 4.14.5 [ ] `BlockLegacyClientPlugin`: `403` for desktop sync clients (`mirall/X.Y.Z` UA) below `minimum.supported.desktop.version` or above `maximum.supported.desktop.version` from `oc_appconfig`
- 4.14.6 [x] `DummyGetResponsePlugin`: `GET` on a DAV collection → `200` plain-text body (not HTML or 404)
- 4.14.7 [x] `RequestIdHeaderPlugin`: mirror incoming `X-Request-Id` onto every DAV response (generate UUID if absent)
- 4.14.8 [x] `UserIdHeaderPlugin`: `X-Nextcloud-User-Id: {uid}` on every authenticated DAV response
- 4.14.9 [ ] `FilesDropPlugin`: public-share method enforcement (PUT/MKCOL/MOVE only) — **deferred to Phase 6** (no `/public.php/dav` handler yet)

**Verify:** `build/integration/dav_features/principal-property-search.feature` for AppleQuirks. Manual: browser GET on `/remote.php/webdav` → plain text. Anonymous OPTIONS from MS Office UA → 200. Authenticated PROPFIND → `X-Nextcloud-User-Id` header present.

### 4.15 WebDAV litmus compliance
- [ ] Run full `litmus` test suite against the running server
- [ ] Zero failures

**Verify:** `litmus http://localhost:7000/remote.php/webdav admin password` exits 0.
