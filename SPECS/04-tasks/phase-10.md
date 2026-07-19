# Phase 10 — PHP-Parity Discrepancy Remediation

Goal: fix behaviours in **already-implemented** Phase 4/5 tasks that were found to deviate from the canonical PHP reference during grounding. Each item is a small, targeted correction to existing Rust code (not new functionality), with the exact PHP source it must match.

> **Scope rule:** these are parity fixes for code that is already marked `[x]` in earlier phases. The original task stays checked; the corrected behaviour is tracked and verified here. All PHP paths are relative to the reference source root (`~/Git/nc-server`).

---

### 10.1 `X-User-Id` response header (from §4.14.8)
> PHP source: `apps/dav/lib/Connector/Sabre/UserIdHeaderPlugin.php:33` — `$response->setHeader('X-User-Id', $user->getUID());`

- [x] The authenticated-response header is `X-User-Id`, **not** `X-Nextcloud-User-Id`. Rust currently emits `x-nextcloud-user-id` (`nc-dav/src/handler.rs:39` `H_X_NC_USER_ID`, also `nc-dav/src/archive.rs:217`). Rename the emitted header to `X-User-Id` on every authenticated DAV response
- [x] Update the §4.14.8 task text and REQ §14.3 to the correct header name
- [x] Grep for any other emitter / test asserting `x-nextcloud-user-id` and reconcile

**Verify:** authenticated `PROPFIND` response carries `X-User-Id: {uid}` (matching PHP); no `X-Nextcloud-User-Id` header is emitted. `build/integration/dav_features/` header assertions pass.

### 10.2 Fake lock token format (from §4.13)
> PHP source: `apps/dav/lib/Connector/Sabre/FakeLockerPlugin.php:114` — `$lockInfo->token = md5($request->getPath());` (raw 32-char MD5 hex, **no** prefix; timeout `1800` at `:117`).

- [x] Rust issues `urn:uuid:{md5_hex(path)}` (`nc-dav/src/locksystem.rs:48`, and the doc comments at `:8`, `:43`, `:59` claim "matching the PHP `FakeLockerPlugin`" — which is inaccurate). PHP returns the **raw** `md5(path)` with no `urn:uuid:` prefix. Either (a) emit the raw `md5(path)` to match PHP exactly, or (b) if the prefix is retained deliberately, correct the misleading "matches PHP" comments and note the intentional deviation
- [x] Reconcile the §4.13 task line (`urn:uuid:{md5_hex(path)}`) with whichever decision is taken

**Verify:** `LOCK` response `{DAV:}lockdiscovery` carries the chosen token; `litmus` lock tests pass. If matching PHP, the `<d:locktoken>` value is a bare 32-char hex string.

> **Note on impact:** low — the fake locker is stateless (`check()` always validates), so the token value never affects request handling. This is a cosmetic/parity fix, mainly so the code comments stop over-claiming PHP parity.

### 10.3 Filename-validation status code `400`, not `422` (from §5.1)
> PHP source: forbidden/invalid filenames throw `OCP\Files\InvalidPathException`, caught in `apps/dav/lib/Connector/Sabre/Directory.php:133-134` (and `:164`, `:209`) and re-thrown as `Connector\Sabre\Exception\InvalidPath`, whose `getHTTPCode()` returns **`400`** (`apps/dav/lib/Connector/Sabre/Exception/InvalidPath.php:33`).

- [x] Rust returns **`422 Unprocessable Entity`** for filename violations (`nc-dav/src/handler.rs:117-138` `build_filename_error_response`; `nc-db/src/filename_validator.rs:24`). PHP returns **`400 Bad Request`**. Change the filename-rejection response status to `400` to match PHP (keep the `{DAV:}error` / `<o:reason>` body)
- [x] Update the §5.1 task text (`422 Unprocessable Entity` → `400 Bad Request`) and REQ §7 accordingly

**Verify:** PUT/MKCOL/MOVE/COPY of a forbidden name (e.g. `.htaccess`) → `400` (matching PHP); `build/integration/dav_features/webdav-related.feature` filename scenarios pass. Confirm the DAV error body still carries the reason element.

### 10.4 Bulk-upload per-part hash validation (from §5.9) — MISSING behaviour
> PHP source: `apps/dav/lib/BulkUpload/MultipartRequestParser.php:141-146` — `parseNextPart()` reads `content-length` (required → `LengthRequired` if absent) and calls `validateHash($length, $headers['x-file-md5'] ?? '', $headers['oc-checksum'] ?? '')` before returning the part content.

- [x] Validate each bulk part against its `X-File-MD5` header (MD5 of the part content) and/or `OC-Checksum` header (standard `{ALG}:{hash}` format) — reject the part (or request) on mismatch, matching PHP `MultipartRequestParser::validateHash()`. Rust's bulk handler (`nc-dav/src/bulk_handler.rs`) currently performs **no** per-part hash check (grep: no `x-file-md5` handling)
- [x] Per-part `Content-Length` is **required**; a missing length is a `411 Length Required` (PHP `LengthRequired`)

**Verify:** bulk POST with a part whose `X-File-MD5` mismatches the body → that part reports an error (and/or `400`), matching PHP; a part missing `Content-Length` → `411`.

> **Deviations:** the implementation follows PHP's actual HTTP behavior, not the exception class name. PHP throws `Sabre\DAV\Exception\LengthRequired` (HTTP 411) for missing `Content-Length`, but `BulkUploadPlugin::httpPost()` catches all `\Exception` and sets status to `400` (not 411) with partial JSON results. Rust matches the observed HTTP status (400 for all parse/hash errors, partial results returned). Hash comparison is case-sensitive (`!=`) matching PHP's `!==`. Supported algorithms are `md5`, `sha1`/`sha-1`, `sha256`/`sha-256`, `sha384`/`sha-384`, `sha512`/`sha-512`, `adler32`/`adler-32` — PHP's `hash_init()` supports arbitrary registered algorithms, but these cover all checksums used by Nextcloud desktop/mobile clients.

### 10.5 `X-OC-MTime` / `X-OC-CTime` sanitization (from §5.3/§5.7/§5.9) — MISSING validation
> PHP source: `apps/dav/lib/Connector/Sabre/MtimeSanitizer.php:12-38` — `sanitizeMtime()` throws `InvalidArgumentException` when the value is **non-numeric or hexadecimal**, or when the integer is **`<= 86400`** ("must be a valid positive unix timestamp greater than one day"). Called from `File.php:348,361`, `ChunkingV2Plugin.php:293`, and bulk (`MtimeSanitizer`).

- [ ] Sanitize every client-supplied `X-OC-MTime` / `X-OC-CTime` (simple PUT, chunked MOVE assembly, bulk parts): reject non-numeric/hex values and any timestamp `<= 86400` before honouring it. Rust currently accepts the header value without these bounds (no `86400` / numeric guard in the mtime path)
- [ ] Match PHP's failure mode (an invalid mtime is **not** silently accepted); confirm whether PHP fails the write or skips the touch, and mirror it

**Verify:** PUT with `X-OC-MTime: 5` or `X-OC-MTime: 0x10` → rejected/not honoured exactly as PHP; PUT with a valid `> 86400` timestamp → honoured with `X-OC-MTime: accepted`.

### 10.6 Quota enforcement on MKCOL / MOVE / COPY (from §5.2) — MISSING coverage
> PHP source: `apps/dav/lib/Connector/Sabre/QuotaPlugin.php` registers `beforeWriteContent`, `beforeCreateFile`, `beforeMove`, `beforeCopy` (`:61-65`) **and** `onCreateCollection` for MKCOL (`:114-128`). MKCOL uses a fixed **4096-byte** assumption (`:124`); `beforeMove`/`beforeCopy` check the **source node's size** against the destination parent's free space (`:154-190`). `getLength()` = largest of `X-Expected-Entity-Length` / `Content-Length` / `OC-Total-Length` (`:270`); negative/unknown free space → allowed; over → `InsufficientStorage` (`507`).

- [ ] Rust enforces quota **only** for `PUT` (`nc-dav/src/handler.rs:162` `if req_method == Method::PUT`). Extend the check to:
  - **COPY** — source size vs destination-parent free space (the clearest gap: a `COPY` of a large file into a near-full folder must `507`)
  - **MOVE** — same, for cross-storage / chunk-assembly (`FutureFile`) moves (home-to-home rename consumes no new space, so only cross-storage matters)
  - **MKCOL** — fixed `4096`-byte check
- [ ] Preserve the §5.2 semantics already correct for PUT (largest length header; negative free space skips; `507` on exceed)

**Verify:** `COPY` a file larger than remaining quota into a quota-limited folder → `507` (matching PHP); `MKCOL` at exactly-full quota → `507`; normal MOVE within home → unaffected.

### 10.7 `downloadStartSecret` → `ocDownloadStarted` cookie (from §4.3) — MISSING behaviour
> PHP source: `apps/dav/lib/Connector/Sabre/FilesPlugin.php:225-239` (`httpGet`) — when a GET carries a `downloadStartSecret` query parameter that is `<= 32` alphanumeric chars, PHP sets a short-lived cookie `ocDownloadStarted={token}` (`time() + 20`, path `/`).

- [ ] On `GET /dav/files/{userId}/…?downloadStartSecret={token}`: if `token` is `<= 32` chars and matches `^[a-zA-Z0-9]+$`, set `Set-Cookie: ocDownloadStarted={token}; Max-Age=20; Path=/`. Rust GET has no handling for this (grep: none). Used by the web Files app to detect that a download has begun (clears the "preparing download" state)
- [ ] Ignore silently when the parameter is absent or invalid (no cookie, no error)

**Verify:** `GET …?downloadStartSecret=abc123` → response carries `Set-Cookie: ocDownloadStarted=abc123` (Max-Age 20); an over-long or non-alphanumeric token sets no cookie; the web UI download-progress indicator clears.

### 10.8 Wrong `oc_filecache.mimepart` on every write (from §4.4/§4.5/§5.7/§5.9) — DB-interop bug
> PHP source: `lib/private/Files/Cache/Cache.php:466` — `mimepart = mimetypeLoader->getId(substr($mimetype, 0, strpos($mimetype, '/')))`, i.e. the part is stored **without** a trailing slash (`image`, `httpd`, …). Mimetype filtering queries by it: `Cache.php:227` `WHERE mimepart = {id}`.

- [ ] Rust resolves the mimepart id with a **trailing slash** — `cache.get_id(&format!("{part}/"))` — which never matches the slash-less `oc_mimetypes` key, so it falls back to `1`. Fix all three write paths to look up the part **without** the slash (`cache.get_id(part_str)`): PUT (`nc-dav/src/filesystem.rs:767`), bulk upload (`nc-dav/src/bulk_handler.rs:243`), chunked-assembly (`nc-dav/src/upload_handler.rs:516`)
- [ ] Directory creation (`nc-dav/src/filesystem.rs:270`) stores the full `httpd/unix-directory` id as `mimepart`; PHP uses `getId('httpd')`. Set the dir `mimepart` to the id of `httpd` (the part), not the full type
- [ ] If the part **or full-mimetype** id is genuinely absent from `oc_mimetypes`, **insert it** (PHP `MimeTypeLoader::getId` auto-inserts) rather than defaulting to `1` — the full-type lookup `cache.get_id(&mime_str).unwrap_or(1)` (`bulk_handler.rs:241`, `filesystem.rs:766`, `upload_handler.rs:515`) has the same fallback bug for uncommon/new types

**Verify:** PUT an `image/png`; `SELECT mimetype, mimepart FROM oc_filecache WHERE …` shows `mimepart` = id of `image` (matching a PHP-uploaded image). The web Files "media"/photos filters (which query `WHERE mimepart = {image_id}`) include Rust-uploaded files. Create a folder → its `mimepart` = id of `httpd`.

### 10.9 `fileid` allocation collides with PHP's DB sequence (from §4.4/§4.5/§5.x) — DB-integrity bug (Postgres)
> PHP source: `core/Migrations/Version13000Date20170718121200.php:156-158` — `oc_filecache.fileid` is `BIGINT` with `'autoincrement' => true`, i.e. a **sequence** on Postgres/MySQL. PHP inserts allocate via that sequence.

- [x] Rust allocates `fileid` with `SELECT COALESCE(MAX(fileid), 0) + 1` (`nc-db`/`nc-dav/src/row.rs:426-427`, used by every filecache insert: `davfile.rs:372`, `filesystem.rs:192,907,1194`, `bulk_handler.rs:269`, `upload_handler.rs:489`). An **explicit** id insert does **not** advance the Postgres sequence, so the next PHP-FPM insert (`files_versions` version rows, trash restore, any PHP file op) draws `nextval` = an id Rust already used → **duplicate-key violation** and the PHP operation fails
- [x] Fix: allocate `fileid` from the DB's own sequence to match PHP — on Postgres `INSERT … (fileid, …) VALUES (DEFAULT, …) RETURNING fileid` (or `nextval(pg_get_serial_sequence('oc_filecache','fileid'))`); MySQL `AUTO_INCREMENT` + `LAST_INSERT_ID()`; SQLite `INTEGER PRIMARY KEY` rowid. Never hand-pick `MAX+1` on a PHP-owned schema
- [x] Update the isolated test schema (`migrations/0003_filecache.sql`) so `fileid` auto-increments there too, keeping the fix engine-consistent

**Verify:** with a PHP-created DB, upload files via Rust, then trigger a PHP-FPM filecache insert (e.g. create a version, or `occ files:scan`) → no duplicate-key error; `SELECT last_value FROM oc_filecache_fileid_seq` stays ahead of `MAX(fileid)`.

> **Implementation note:** The migration uses `INTEGER` (not `BIGINT`) because it only runs against SQLite tests, where `INTEGER PRIMARY KEY` is required for auto-increment. Production PostgreSQL has `BIGSERIAL` from PHP Doctrine.

### 10.10 `MOVE`/`COPY` does not update `mimetype` on extension change (from §4.x) — stale type
> PHP source: `lib/private/Files/Cache/Updater.php:181-186` (`copyOrRenameFromStorage`) — when `sourceExtension !== targetExtension` and the node is a file (and not a trash move), PHP recomputes the mimetype (`storage->getMimeType(target)`) and `cache->update(fileId, ['mimetype' => …])`.

- [ ] Rust's `rename` (`nc-dav/src/filesystem.rs:1105-1108`) updates `path/path_hash/name/parent/mtime/etag` but **not** `mimetype`/`mimepart`. Renaming across an extension change (e.g. `note.txt` → `photo.jpg`) leaves the old `text/plain` type in `oc_filecache`, so `{DAV:}getcontenttype`, the web icon/preview, and type filters stay wrong
- [ ] On `MOVE`/`COPY` where the target extension differs from the source, recompute `mimetype`+`mimepart` from the target name and update the row (skip for directories and trash `.d{ts}` targets, matching PHP's `$targetIsTrash` guard)

**Verify:** `MOVE note.txt → photo.jpg`; PROPFIND `{DAV:}getcontenttype` on `photo.jpg` = `image/jpeg` and `oc_filecache.mimetype`/`mimepart` reflect the new type.

### 10.11 Custom DAV property storage (`oc_properties`) — MISSING behaviour
> PHP source: `Sabre\DAV\PropertyStorage\Plugin` backed by `apps/dav/lib/Connector/Sabre/CustomPropertiesBackend.php`, registered for logged-in users in `apps/dav/lib/Server.php`. Any PROPPATCH property not consumed by another plugin is persisted to `oc_properties` (`userid`, `propertypath`, `propertyname`, `propertyvalue`, `valuetype`) and returned on PROPFIND.

- [x] Rust `patch_props` returns `403 Forbidden` for every property outside its known set (`nc-dav/src/filesystem.rs:1591` `_ => FORBIDDEN`) and has **no** `oc_properties` handling at all. PHP stores unknown props and returns `200`. Add a custom-properties fallback: PROPPATCH-set an unknown prop → upsert into `oc_properties` → `200`; PROPPATCH-remove → delete the row; PROPFIND → include stored custom props for the node
- [x] Clean up on file lifecycle: on hard `DELETE` remove the node's `oc_properties` rows; on `MOVE`/rename update `propertypath` (PHP's backend moves/deletes them). Rust's delete/move currently ignore `oc_properties`, orphaning or stranding rows written by PHP-FPM
- [x] Keep the known-property handlers (etag/lastmodified/creationdate/creation_time/upload_time, displayname→403) taking precedence over the custom store

**Verify:** `PROPPATCH` a custom prop (e.g. `{urn:example}state`) on a file → `200`; a subsequent PROPFIND returns it (matching PHP). Delete the file (hard delete) → its `oc_properties` rows are gone. macOS Finder / third-party clients that store extended attributes as DAV props round-trip correctly.

> **Deviation:** PHP's `cacheDirectory`/`cacheCalendars` prefetch optimization not implemented (batch JOIN on `oc_filecache` to preload all children's props at once — performance, not correctness). PHP's `PUBLISHED_READ_ONLY_PROPERTIES` (CalDAV calendar-availability) not implemented — only relevant for calendar/addressbook trees, not the files tree served by Rust. PHP's `PROPERTY_DEFAULT_VALUES` (calendar-enabled=1 → delete row) not implemented — CalDAV-specific. PHP's `ALLOWED_NC_PROPERTIES` per-property whitelist simplified to a blanket oc/nc namespace filter in PROPFIND (same effect for the files tree). PHP's `PROPERTY_TYPE_OBJECT` (PHP-serialized objects) not supported — Rust can't unserialize PHP objects; only `valuetype=2` (XML) is stored. Known-property precedence is maintained by matching all known namespaces in the PROPPATCH DELETE arm and filtering known namespaces from the PROPFIND custom-props loop.

### 10.12 `{nc:}has-preview` hardcoded `false` → no web-UI thumbnails (from §4.9)
> PHP source: `apps/dav/lib/Connector/Sabre/FilesPlugin.php:392-393` — `has-preview` = `json_encode($previewManager->isAvailable($node->getFileInfo()))`, i.e. `true` when a registered preview provider supports the file's mimetype (and `enable_previews` is on).

- [ ] Rust emits a constant `"false"` for `{nc:}has-preview` (`nc-dav/src/props.rs`, `make_prop("has-preview", "nc", NC_NS, "false")`). The web Files app uses this flag to decide whether to request a thumbnail — so **every** file shows a generic icon and no image/video thumbnails render in grid/photos views
- [ ] Compute it from the mimetype against the enabled preview providers (config `enabledPreviewProviders`, default set covers `image/*`, `video/*`, `application/pdf`, text, office/OpenDocument, SVG, …) gated on the `enable_previews` config (default on). Thumbnail *serving/generation* is designed in [`phase-11.md`](phase-11.md) — this property only tells the client a preview is available
- [ ] Shared with **Phase 11.1** (it is the prerequisite for the native preview fast path); completing it in either place satisfies both. Keep the two cross-referenced.

**Verify:** PROPFIND an image → `{nc:}has-preview` = `true`; a `.bin` → `false`. Web Files grid view renders image thumbnails for Rust-served files.

---

### Out of scope (intentional)

- **Object-store multipart uploads** — `ChunkingV2Plugin` has a native S3/object-store path (`IObjectStoreMultiPartUpload`: `startChunkedWrite`/`putChunkedWritePart`/`completeChunkedWrite`, `ChunkingV2Plugin.php` `beforeMove`). The Rust implementation targets **local disk** storage; object-store multipart is not reimplemented. Document as an intentional non-goal (revisit only if object storage becomes a target).
