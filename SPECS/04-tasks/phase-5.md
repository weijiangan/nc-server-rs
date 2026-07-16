# Phase 5 — Upload Flows

Goal: all upload methods used by desktop and mobile clients work end-to-end, including quota and filename validation.

---

### 5.1 Filename validation
- [x] Before any write (PUT, MKCOL, MOVE destination, COPY destination): validate against `forbidden_filenames`, `forbidden_filename_basenames`, `forbidden_filename_characters`, `forbidden_filename_extensions` from app config cache
- [x] Violation → `422 Unprocessable Entity`
- [x] Validation reads from the app config cache (Phase 0.6) — no DB query per upload

**Verify:** attempt PUT of a file named `.htaccess` → `422`. PUT of a valid name → proceeds normally.

### 5.2 Quota enforcement
- [x] Before write: resolve `free_space()` for target storage
- [x] Compare against `max(Content-Length, X-Expected-Entity-Length, OC-Total-Length)`
- [x] Negative `free_space()` (any of `SPACE_NOT_COMPUTED=-1`, `SPACE_UNKNOWN=-2`, `SPACE_UNLIMITED=-3`) → skip check, allow write
- [x] Quota exceeded → `507 Insufficient Storage`

**Verify:** set a user quota of 1 MB in `oc_preferences`; attempt PUT of 2 MB → `507`. Set quota to unlimited → PUT succeeds regardless of size.

### 5.3 Simple PUT upload
- [x] `PUT` with `Content-Type: application/octet-stream`
- [x] Optional `X-OC-MTime` honored; response includes `X-OC-MTime: accepted`
- [x] Optional `X-OC-CTime` honored; response includes `X-OC-CTime: accepted`
- [x] Optional `OC-Checksum: {ALG}:{hash}` validated against computed hash; mismatch → `400 Bad Request`; match → stored in `oc_filecache.checksum`
- [x] `201 Created` for new file, `204 No Content` for overwrite
- [x] Response: `OC-FileId`, `ETag`, `OC-ETag`

**Verify:** `build/integration/files_features/checksums.feature` — upload with checksum, mismatch, and correct checksum scenarios.

### 5.4 Chunked upload v1 (`OC-Chunked`) ~~CUT~~
- [x] Requests with `OC-Chunked: 1` header return `501 Not Implemented` and are logged
> **Cut:** OC-Chunked was an older desktop sync client protocol (pre-3.0, 2020). It was never used by the web browser or mobile clients, and modern desktop clients default to Chunked v2 or simple PUT. Requests with the `OC-Chunked: 1` header return `501 Not Implemented` and are logged; no client in the target set will send them.

### 5.5 Chunked upload v2 — MKCOL (phase 1)
- [x] `MKCOL /dav/uploads/{userId}/{upload_id}` with `Destination` header required
- [x] Store `(upload_id, target_path)` in distributed cache or local in-process store
- [x] `201 Created`
- [x] If no distributed cache configured: proceed with in-process map (disable v2 capability only if this is a known limitation)

**Verify:** `build/integration/dav_features/dav-v2.feature` — MKCOL upload slot creation.

### 5.6 Chunked upload v2 — PUT chunks (phase 2)
- [x] `PUT /dav/uploads/{userId}/{upload_id}/{part_id}` where `part_id` is numeric 1–10000
- [x] Reject `part_id < 1` or `part_id > 10000` → `400`
- [x] Write chunk to temp area keyed by `(upload_id, part_id)`
- [x] Quota enforced on chunk PUT when Content-Length is known (§5.2)

**Verify:** `build/integration/dav_features/dav-v2.feature` — chunk PUT scenarios. Confirm invalid part ID rejected.

### 5.7 Chunked upload v2 — MOVE assembly (phase 3)
- [x] `MOVE /dav/uploads/{userId}/{upload_id}/.file` with `Destination` header
- [x] Optional `OC-Total-Length`: if present, validate sum of chunk sizes matches
- [x] Assemble chunks in `part_id` order, write to `Destination`
- [x] Optional `X-OC-MTime` / `X-OC-CTime` honored
- [x] `201 Created` or `204 No Content`
- [x] Quota enforced before assembly (§5.2)
- [x] `upload_time` set in `oc_filecache_extended` for new files

**Verify:** full three-phase chunked v2 upload; confirm assembled file matches original. Test with wrong `OC-Total-Length` → `400`.

### 5.8 Chunked upload v2 — DELETE (abort)
- [x] `DELETE /dav/uploads/{userId}/{upload_id}` removes all temp chunks and cache entry
- [x] `204 No Content`

**Verify:** start upload, abort with DELETE, confirm temp files removed.

### 5.9 Bulk upload (`POST /dav/bulk`)
- [x] Parse `multipart/related` body; per-part headers `X-File-Path`, `X-OC-MTime` / `X-File-MTime`, `Content-Length`
- [x] Write each part to the correct path
- [x] Response: JSON map of path → `{error, etag, fileid, permissions}`
- [x] Partial failure: write what succeeded, include errors for failed parts in response
- [x] Quota enforced per file before write (§5.2)
- [x] `fileid` formatted per PHP `DavUtil::getDavFileId()` (zero-padded 8 char + instanceId)
- [x] `upload_time` set in `oc_filecache_extended` for new files

**Verify:** `build/integration/dav_features/dav-v2.feature` bulk upload scenario (if present); otherwise integration test: upload 5 files in one bulk request, confirm all present with correct etags.

### 5.10 ZIP/TAR folder download
- [x] `GET /dav/files/{userId}/{folder}` with `Accept: application/zip` or `?accept=zip` → streamed ZIP; `Content-Disposition: attachment; filename="foldername.zip"`
- [x] Same endpoint with `Accept: application/x-tar` or `?accept=tar` → streamed TAR archive (REQ §7.5)
- [x] Root folder download named `download.zip` / `download.tar` respectively
- [x] Optional `?files=["name1","name2"]` or `X-NC-Files` headers to filter children
- [x] Dual-mode: buffered (≤10 MiB) with Content-Length, streaming (>10 MiB) with chunked transfer

**Verify:** `build/integration/files_features/download.feature` — ZIP and TAR download scenarios. Download a folder via each format, extract, verify contents match.

### 5.11 Checksum recalculation (`PATCH`) — STRETCH GOAL
> **Decision — delegate to PHP-FPM (do not implement natively):** `X-Recalculate-Hash` is a rare admin/integrity operation, off the sync hot path, and a discrete request PHP-FPM can serve end-to-end. **Forward `PATCH` on `/dav/files/…` to PHP-FPM** rather than implementing it in Rust (verify the files-tree router forwards `PATCH` instead of returning `405`). The grounded detail below is retained for reference and for the case PHP-FPM is later removed.
> PHP source: `apps/dav/lib/Connector/Sabre/ChecksumUpdatePlugin.php` (registered on `method:PATCH` at `:21`; `httpPatch` at `:33`).

- [ ] `PATCH /dav/files/{userId}/{path}` with header `X-Recalculate-Hash: {algorithm}` — handled **only** when the path resolves to a **file** node (`$node instanceof File`, `:37`); a directory or non-file path falls through to the default handler (no recalculation)
- [ ] The algorithm from the header is **lowercased** for hashing (`strtolower`, `:38-40`), then the stored file is re-hashed via `File::hash($type)` (`:42`); supported algorithms match the PUT-time set (§4.4): `md5`, `sha1`, `sha256`, `adler32`
- [ ] On success: persist `oc_filecache.checksum = {ALG}:{hash}` with `ALG` **uppercased** (e.g. header `sha256` → `SHA256:…`) via `setChecksum` (`:44-45`)
- [ ] Response: `204 No Content` with `OC-Checksum: {ALG}:{new_hash}` and `Content-Length: 0` (`:46-48`)
- [ ] If `File::hash()` returns empty (unknown algorithm / storage can't hash), do **nothing** — fall through to default `PATCH` handling; no `204`, checksum unchanged (`:43`)

**Verify:** `build/integration/files_features/checksums.feature` — PATCH recalculation scenario. Assert `PATCH` with `X-Recalculate-Hash: sha256` on a file → `204` + `OC-Checksum: SHA256:…`; the same on a directory → not recalculated.

---

## Deviations from PHP Reference

Documented differences between the Rust implementation and the PHP reference discovered during implementation and validated against `apps/dav/lib/Upload/ChunkingV2Plugin.php`, `apps/dav/lib/BulkUpload/BulkUploadPlugin.php`, and `lib/public/Files/DavUtil.php`.

### Global server vs local file semantics
- **PHP:** Chunked upload v2 requires a **distributed cache** (Redis/Memcached). `ChunkingV2Plugin::checkPrerequisites()` throws `BadRequest` if `memcache.distributed` is `null` or the cache is not Redis/Memcached, effectively disabling v2.
- **Rust:** Falls back to an in-process `RwLock<HashMap>` (`upload.rs`) when no distributed cache is configured. This enables single-node operation but won't survive process restarts. The v2 capability is still advertised regardless.

### MOVE assembly: `.file` suffix
- **REQ §7.3** documents `MOVE /dav/uploads/{userId}/{upload_id}/.file`.
- **PHP `ChunkingV2Plugin::beforeMove()`** does **not** validate the `.file` suffix. It extracts the upload folder via `dirname($sourcePath)` and assembles regardless of the final path segment.
- **Rust:** Follows PHP — any MOVE on a path within an upload folder triggers assembly.

### MIME type detection
- **PHP `ChunkingV2Plugin::beforeMove()` (line 204):** Uses `IMimeTypeDetector::detectPath($destinationName)` — a file-content-based detector.
- **Rust:** Uses `mime_guess::from_ext()` — extension-based heuristic. This may produce different MIME types for files with ambiguous or missing extensions.

### Filename validation
- **PHP:** Chunked upload and bulk upload do **not** validate filenames at the plugin level. Validation occurs downstream in the storage/filesystem layer (`$userFolder->newFile()` → hooks → `IFilenameValidator`).
- **Rust:** The main `dav_handler` validates filenames for simple PUT (§5.1). The chunked upload and bulk upload handlers do not validate filenames — matching PHP's approach of relying on the storage layer.

### Checksum validation
- **PHP:** Neither `ChunkingV2Plugin` nor `BulkUploadPlugin` performs checksum validation. The `OC-Checksum` header is handled by the storage layer or `ChecksumUpdatePlugin` during actual file writes.
- **Rust:** The main `dav_handler` validates checksums for simple PUT via `NcDavFile::flush()`. The chunked upload and bulk upload handlers do not validate checksums — matching PHP's approach.

### Parent directory auto-creation scope
- **PHP:** `View::createParentDirectories()` is called from `$userFolder->newFile()` and `$userFolder->newFolder()` — so simple PUT and MKCOL auto-create missing parent directories in the filecache. However, chunked upload v2 assembly (`ChunkingV2Plugin::beforeMove()`) does **not** call `createParentDirectories()`. If a chunked upload targets a path whose parent does not already exist in the filecache, assembly fails with a "Target folder does not exist" error.
- **Rust:** `ensure_parent_dir()` is called uniformly from all write paths — simple PUT (`open()`), MKCOL (`create_dir()`), and chunked upload v2 assembly. This means chunked upload to a path with a non-existent parent succeeds in Rust but fails in PHP. The Rust behavior is the intended behavior (parent auto-creation should not depend on which upload method the client uses), but it is documented here as a behavioral difference. Note that both implementations are affected identically by the client-side bug where `createDirectoryIfNotExists` sends MKCOL to the DAV root instead of the upload destination path — the empty root directory artifact is not server-specific.

