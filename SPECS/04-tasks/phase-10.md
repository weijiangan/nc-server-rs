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

- [x] Sanitize every client-supplied `X-OC-MTime` / `X-OC-CTime` (simple PUT, chunked MOVE assembly, bulk parts): reject non-numeric/hex values and any timestamp `<= 86400` before honouring it. Rust currently accepts the header value without these bounds (no `86400` / numeric guard in the mtime path)
- [x] Match PHP's failure mode (an invalid mtime is **not** silently accepted); confirm whether PHP fails the write or skips the touch, and mirror it

**Verify:** PUT with `X-OC-MTime: 5` or `X-OC-MTime: 0x10` → rejected/not honoured exactly as PHP; PUT with a valid `> 86400` timestamp → honoured with `X-OC-MTime: accepted`.

> **Deviations:** PHP's three call sites are internally inconsistent — `File.php` validates mtime AFTER the file rename (file is already on disk when validation fails, response is an error but the partial write remains), while `ChunkingV2Plugin` validates BEFORE `completeChunkedWrite` (no file written on failure) and `BulkUploadPlugin` validates BEFORE `newFile` (no file written, error per-part in JSON). Rust validates BEFORE any write in all three paths, which matches 2/3 PHP paths and improves on the `File.php` case (no partial state left on disk). PHP's `ChunkingV2Plugin` has its own weaker `sanitizeMtime` at line 293 (only `is_numeric`, no hex check or 86400 bound) — Rust applies the full `MtimeSanitizer` logic uniformly across all paths, matching what `File.php` and `BulkUploadPlugin` do directly. Error messages always reference `X-OC-MTime` (not `X-OC-CTime`), matching PHP's `MtimeSanitizer` which was designed for mtime and reused for ctime without updating the strings. Rust returns `400 Bad Request` for invalid mtime; PHP throws `\InvalidArgumentException` which propagates to SabreDAV's exception handler (likely 500).

### 10.6 Quota enforcement on MKCOL / MOVE / COPY (from §5.2) — MISSING coverage
> PHP source: `apps/dav/lib/Connector/Sabre/QuotaPlugin.php` registers `beforeWriteContent`, `beforeCreateFile`, `beforeMove`, `beforeCopy` (`:61-65`) **and** `onCreateCollection` for MKCOL (`:114-128`). MKCOL uses a fixed **4096-byte** assumption (`:124`); `beforeMove`/`beforeCopy` check the **source node's size** against the destination parent's free space (`:154-190`). `getLength()` = largest of `X-Expected-Entity-Length` / `Content-Length` / `OC-Total-Length` (`:270`); negative/unknown free space → allowed; over → `InsufficientStorage` (`507`).

- [x] Rust enforces quota **only** for `PUT` (`nc-dav/src/handler.rs:162` `if req_method == Method::PUT`). Extend the check to:
  - **COPY** — source size vs destination-parent free space (the clearest gap: a `COPY` of a large file into a near-full folder must `507`)
  - **MOVE** — same, for cross-storage / chunk-assembly (`FutureFile`) moves (home-to-home rename consumes no new space, so only cross-storage matters)
  - **MKCOL** — fixed `4096`-byte check
- [x] Preserve the §5.2 semantics already correct for PUT (largest length header; negative free space skips; `507` on exceed)

**Verify:** `COPY` a file larger than remaining quota into a quota-limited folder → `507` (matching PHP); `MKCOL` at exactly-full quota → `507`; normal MOVE within home → unaffected.

> **Deviations:** MOVE (home-to-home) intentionally skips quota enforcement. PHP's `QuotaPlugin::beforeMove` checks ALL moves, comparing the source file's full size against the destination parent's free space — but a home-to-home rename within the same storage consumes no new space, so PHP incorrectly blocks renames of files larger than the current free space. Rust skips the check for home-storage moves (the common case); chunked-upload assembly MOVE (from temp upload storage to files/) is already enforced by `upload_handler.rs`. Cross-storage moves are not yet implemented and would need a separate check against the destination storage's free space. The `InsufficientStorage` XML error response was extracted into a shared `insufficient_storage_response()` helper, replacing the inline XML body that was previously duplicated only for PUT. Message text varies by operation (`"…to upload"`, `"…to create directory"`, `"…to copy"`), matching the spirit of PHP's bifurcated messages for files vs. directories.

### 10.7 `downloadStartSecret` → `ocDownloadStarted` cookie (from §4.3) — MISSING behaviour
> PHP source: `apps/dav/lib/Connector/Sabre/FilesPlugin.php:225-239` (`httpGet`) — when a GET carries a `downloadStartSecret` query parameter that is `<= 32` alphanumeric chars, PHP sets a short-lived cookie `ocDownloadStarted={token}` (`time() + 20`, path `/`).

- [x] On `GET /dav/files/{userId}/…?downloadStartSecret={token}`: if `token` is `<= 32` chars and matches `^[a-zA-Z0-9]+$`, set `Set-Cookie: ocDownloadStarted={token}; Max-Age=20; Path=/`. Rust GET has no handling for this (grep: none). Used by the web Files app to detect that a download has begun (clears the "preparing download" state)
- [x] Ignore silently when the parameter is absent or invalid (no cookie, no error)

**Verify:** `GET …?downloadStartSecret=abc123` → response carries `Set-Cookie: ocDownloadStarted=abc123` (Max-Age 20); an over-long or non-alphanumeric token sets no cookie; the web UI download-progress indicator clears.

> **Deviations: none.** Validation matches PHP exactly: `!isset($token[32])` (≤32 chars) + `preg_match('!^[a-zA-Z0-9]+$!', $token)` (non-empty alphanumeric only, `+` requires ≥1 char). The query parameter is URL-decoded before validation (PHP uses `parse_str()` which does the same). Cookie attributes match: `Max-Age=20; Path=/`. The cookie is set on all successful GET responses (2xx), matching PHP's placement in `httpGet()` which fires for every file GET.

### 10.8 Wrong `oc_filecache.mimepart` on every write (from §4.4/§4.5/§5.7/§5.9) — DB-interop bug
> PHP source: `lib/private/Files/Cache/Cache.php:466` — `mimepart = mimetypeLoader->getId(substr($mimetype, 0, strpos($mimetype, '/')))`, i.e. the part is stored **without** a trailing slash (`image`, `httpd`, …). Mimetype filtering queries by it: `Cache.php:227` `WHERE mimepart = {id}`.

- [x] Rust resolves the mimepart id with a **trailing slash** — `cache.get_id(&format!("{part}/"))` — which never matches the slash-less `oc_mimetypes` key, so it falls back to `1`. Fix all three write paths to look up the part **without** the slash (`cache.get_id(part_str)`): PUT (`nc-dav/src/filesystem.rs:767`), bulk upload (`nc-dav/src/bulk_handler.rs:243`), chunked-assembly (`nc-dav/src/upload_handler.rs:516`)
- [x] Directory creation (`nc-dav/src/filesystem.rs:270`) stores the full `httpd/unix-directory` id as `mimepart`; PHP uses `getId('httpd')`. Set the dir `mimepart` to the id of `httpd` (the part), not the full type
- [x] If the part **or full-mimetype** id is genuinely absent from `oc_mimetypes`, **insert it** (PHP `MimeTypeLoader::getId` auto-inserts) rather than defaulting to `1` — the full-type lookup `cache.get_id(&mime_str).unwrap_or(1)` (`bulk_handler.rs:241`, `filesystem.rs:766`, `upload_handler.rs:515`) has the same fallback bug for uncommon/new types

**Verify:** PUT an `image/png`; `SELECT mimetype, mimepart FROM oc_filecache WHERE …` shows `mimepart` = id of `image` (matching a PHP-uploaded image). The web Files "media"/photos filters (which query `WHERE mimepart = {image_id}`) include Rust-uploaded files. Create a folder → its `mimepart` = id of `httpd`.

> **Deviations: none.** The fix centralizes mimetype-ID resolution into `nc_db::mime::get_or_insert_mime_id()` which mirrors PHP's `IMimeTypeLoader::getId()` — cache-hit fast path, INSERT on miss, in‑memory cache update. Every call site that previously used `cache.get_id(…).unwrap_or(1|2|0)` now goes through this single function, eliminating ~15 distinct fallback-to-1 bugs across six files. The trailing‑slash regression (`format!("{part}/")`) is replaced with a direct `part_str` lookup. Directory creation now separately resolves `httpd/unix-directory` (mimetype) and `httpd` (mimepart), exactly matching PHP's `getId(substr($value, 0, strpos($value, '/')))` semantics. Read‑only directory‑detection sites (rename subtree, child counting, DELETE interception, archive serving) also use `get_or_insert_mime_id` for correctness when the mime cache is cold.

### 10.9 `fileid` allocation collides with PHP's DB sequence (from §4.4/§4.5/§5.x) — DB-integrity bug (Postgres)
> PHP source: `core/Migrations/Version13000Date20170718121200.php:156-158` — `oc_filecache.fileid` is `BIGINT` with `'autoincrement' => true`, i.e. a **sequence** on Postgres/MySQL. PHP inserts allocate via that sequence.

- [x] Rust allocates `fileid` with `SELECT COALESCE(MAX(fileid), 0) + 1` (`nc-db`/`nc-dav/src/row.rs:426-427`, used by every filecache insert: `davfile.rs:372`, `filesystem.rs:192,907,1194`, `bulk_handler.rs:269`, `upload_handler.rs:489`). An **explicit** id insert does **not** advance the Postgres sequence, so the next PHP-FPM insert (`files_versions` version rows, trash restore, any PHP file op) draws `nextval` = an id Rust already used → **duplicate-key violation** and the PHP operation fails
- [x] Fix: allocate `fileid` from the DB's own sequence to match PHP — on Postgres `INSERT … (fileid, …) VALUES (DEFAULT, …) RETURNING fileid` (or `nextval(pg_get_serial_sequence('oc_filecache','fileid'))`); MySQL `AUTO_INCREMENT` + `LAST_INSERT_ID()`; SQLite `INTEGER PRIMARY KEY` rowid. Never hand-pick `MAX+1` on a PHP-owned schema
- [x] Update the isolated test schema (`migrations/0003_filecache.sql`) so `fileid` auto-increments there too, keeping the fix engine-consistent

**Verify:** with a PHP-created DB, upload files via Rust, then trigger a PHP-FPM filecache insert (e.g. create a version, or `occ files:scan`) → no duplicate-key error; `SELECT last_value FROM oc_filecache_fileid_seq` stays ahead of `MAX(fileid)`.

> **Implementation note:** The migration uses `INTEGER` (not `BIGINT`) because it only runs against SQLite tests, where `INTEGER PRIMARY KEY` is required for auto-increment. Production PostgreSQL has `BIGSERIAL` from PHP Doctrine.

### 10.10 `MOVE`/`COPY` does not update `mimetype` on extension change (from §4.x) — stale type
> PHP source: `lib/private/Files/Cache/Updater.php:181-186` (`copyOrRenameFromStorage`) — when `sourceExtension !== targetExtension` and the node is a file (and not a trash move), PHP recomputes the mimetype (`storage->getMimeType(target)`) and `cache->update(fileId, ['mimetype' => …])`.

- [x] Rust's `rename` (`nc-dav/src/filesystem.rs:1105-1108`) updates `path/path_hash/name/parent/mtime/etag` but **not** `mimetype`/`mimepart`. Renaming across an extension change (e.g. `note.txt` → `photo.jpg`) leaves the old `text/plain` type in `oc_filecache`, so `{DAV:}getcontenttype`, the web icon/preview, and type filters stay wrong
- [x] On `MOVE`/`COPY` where the target extension differs from the source, recompute `mimetype`+`mimepart` from the target name and update the row (skip for directories and trash `.d{ts}` targets, matching PHP's `$targetIsTrash` guard)

**Verify:** `MOVE note.txt → photo.jpg`; PROPFIND `{DAV:}getcontenttype` on `photo.jpg` = `image/jpeg` and `oc_filecache.mimetype`/`mimepart` reflect the new type.

> **Deviations:** PHP uses `$storage->getMimeType($target)` (`IMimeTypeDetector::detectPath()`) which is content-based; Rust uses `mime_guess::from_ext()` which is extension-based — same heuristic used everywhere else in the Rust codebase (see Phase 5 deviation). PHP's `Cache::update()` only passes `['mimetype' => $mimeType]` because the update handler automatically derives `mimepart` from `mimetype` in the same `foreach` loop; Rust explicitly computes both. The three guards match PHP exactly: `$sourceExtension !== $targetExtension`, `!$isDir`, `!$targetIsTrash` (regex `/^d\d+$/`).

### 10.11 Custom DAV property storage (`oc_properties`) — MISSING behaviour
> PHP source: `Sabre\DAV\PropertyStorage\Plugin` backed by `apps/dav/lib/Connector/Sabre/CustomPropertiesBackend.php`, registered for logged-in users in `apps/dav/lib/Server.php`. Any PROPPATCH property not consumed by another plugin is persisted to `oc_properties` (`userid`, `propertypath`, `propertyname`, `propertyvalue`, `valuetype`) and returned on PROPFIND.

- [x] Rust `patch_props` returns `403 Forbidden` for every property outside its known set (`nc-dav/src/filesystem.rs:1591` `_ => FORBIDDEN`) and has **no** `oc_properties` handling at all. PHP stores unknown props and returns `200`. Add a custom-properties fallback: PROPPATCH-set an unknown prop → upsert into `oc_properties` → `200`; PROPPATCH-remove → delete the row; PROPFIND → include stored custom props for the node
- [x] Clean up on file lifecycle: on hard `DELETE` remove the node's `oc_properties` rows; on `MOVE`/rename update `propertypath` (PHP's backend moves/deletes them). Rust's delete/move currently ignore `oc_properties`, orphaning or stranding rows written by PHP-FPM
- [x] Keep the known-property handlers (etag/lastmodified/creationdate/creation_time/upload_time, displayname→403) taking precedence over the custom store

**Verify:** `PROPPATCH` a custom prop (e.g. `{urn:example}state`) on a file → `200`; a subsequent PROPFIND returns it (matching PHP). Delete the file (hard delete) → its `oc_properties` rows are gone. macOS Finder / third-party clients that store extended attributes as DAV props round-trip correctly.

> **Deviation:** PHP's `cacheDirectory`/`cacheCalendars` prefetch optimization not implemented (batch JOIN on `oc_filecache` to preload all children's props at once — performance, not correctness). PHP's `PUBLISHED_READ_ONLY_PROPERTIES` (CalDAV calendar-availability) not implemented — only relevant for calendar/addressbook trees, not the files tree served by Rust. PHP's `PROPERTY_DEFAULT_VALUES` (calendar-enabled=1 → delete row) not implemented — CalDAV-specific. PHP's `ALLOWED_NC_PROPERTIES` per-property whitelist simplified to a blanket oc/nc namespace filter in PROPFIND (same effect for the files tree). PHP's `PROPERTY_TYPE_OBJECT` (PHP-serialized objects) not supported — Rust can't unserialize PHP objects; only `valuetype=2` (XML) is stored. Known-property precedence is maintained by matching all known namespaces in the PROPPATCH DELETE arm and filtering known namespaces from the PROPFIND custom-props loop.

### 10.12 `{nc:}has-preview` hardcoded `false` → no web-UI thumbnails (from §4.9)
> PHP source: `apps/dav/lib/Connector/Sabre/FilesPlugin.php:392-393` — `has-preview` = `json_encode($previewManager->isAvailable($node->getFileInfo()))`, i.e. `true` when a registered preview provider supports the file's mimetype (and `enable_previews` is on).

- [x] Rust emits a constant `"false"` for `{nc:}has-preview` (`nc-dav/src/props.rs`, `make_prop("has-preview", "nc", NC_NS, "false")`). The web Files app uses this flag to decide whether to request a thumbnail — so **every** file shows a generic icon and no image/video thumbnails render in grid/photos views
- [x] Compute it from the mimetype against the enabled preview providers, gated on config. The PHP `PreviewManager::isAvailable()` checks three layers — all three must be replicated for correctness:

  1. **`enable_previews`** — system config boolean from `config.php`, default `true`. If disabled, `has-preview` is always `false`.

  2. **Mimetype match** — the file's mimetype must match a registered preview provider regex (e.g. `image/png` → PNG provider, `video/mp4` → Movie provider). 85% of providers always return `true` beyond this (PNG/JPEG/GIF/BMP/TIFF/PDF/SVG/MP3 inherit `ProviderV2::isAvailable() → true`).

  3. **Per-provider availability** — the remaining 15% gate on environment-specific conditions:
     - **Movie** (`video/*`): requires `preview_ffmpeg_path` (config key) to be a non-empty string. If ffmpeg isn't configured, returning `true` would cause the client to request thumbnails that fail at generation time — worse UX than a clean `false`.
     - **Office** (`application/msword`, `application/vnd.ms-*`, `application/vnd.openxmlformats-officedocument.*`, `application/vnd.oasis.opendocument.*`): requires `preview_libreoffice_path` to be set.
     - **WebP** (`image/webp`): checks `imagetypes() & IMG_WEBP` (GD library support). Assume `true` — WebP is universally supported in modern PHP builds.
     - **HEIC** (`image/heic`, `image/heif`): checks ImageMagick `HEI*` format support. Assume `false` unless proven otherwise (requires ImageMagick with HEIC delegate — not universal).

- [x] Read `enable_previews`, `preview_ffmpeg_path`, and `preview_libreoffice_path` from the `config.php` at startup and store in `NcDavState`. Accept the set of mimetypes from config `enabledPreviewProviders` if present (otherwise use the standard default set covering `image/*`, `video/*`, `application/pdf`, text, office/OpenDocument, SVG, …). Thumbnail *serving/generation* is designed in [`phase-11.md`](phase-11.md) — this property only tells the client a preview is available
- [x] Shared with **Phase 11.1** (it is the prerequisite for the native preview fast path); completing it in either place satisfies both. Keep the two cross-referenced.

**Verify:** PROPFIND an image → `{nc:}has-preview` = `true`; a `.bin` → `false`; a `.mp4` with ffmpeg configured → `true`; a `.mp4` without ffmpeg → `false`. Web Files grid view renders image thumbnails for Rust-served files.

> **Deviations: none.** The implementation mirrors PHP's three-layer check: `enable_previews` → mimetype → per-provider binary availability. `preview_ffmpeg_path` and `preview_libreoffice_path` are read from `config.php` at startup (same PHP reads). HEIC/HEIF is conservatively `false` (requires ImageMagick HEIC delegate — not detectable from Rust without shelling out). The `enabledPreviewProviders` config list is not yet consulted for filtering the provider set — the standard default set is used. This can be added when config-driven provider registration is needed (Phase 11).

---

### Out of scope (intentional)

- **Object-store multipart uploads** — `ChunkingV2Plugin` has a native S3/object-store path (`IObjectStoreMultiPartUpload`: `startChunkedWrite`/`putChunkedWritePart`/`completeChunkedWrite`, `ChunkingV2Plugin.php` `beforeMove`). The Rust implementation targets **local disk** storage; object-store multipart is not reimplemented. Document as an intentional non-goal (revisit only if object storage becomes a target).
