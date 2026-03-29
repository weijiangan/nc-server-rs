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
> **Cut:** OC-Chunked was an older desktop sync client protocol (pre-3.0, 2020). It was never used by the web browser or mobile clients, and modern desktop clients default to Chunked v2 or simple PUT. Requests with the `OC-Chunked: 1` header return `501 Not Implemented` and are logged; no client in the target set will send them.

### 5.5 Chunked upload v2 — MKCOL (phase 1)
- [ ] `MKCOL /dav/uploads/{userId}/{upload_id}` with `Destination` header required
- [ ] Store `(upload_id, target_path)` in distributed cache or local in-process store
- [ ] `201 Created`
- [ ] If no distributed cache configured: proceed with in-process map (disable v2 capability only if this is a known limitation)

**Verify:** `build/integration/dav_features/dav-v2.feature` — MKCOL upload slot creation.

### 5.6 Chunked upload v2 — PUT chunks (phase 2)
- [ ] `PUT /dav/uploads/{userId}/{upload_id}/{part_id}` where `part_id` is numeric 1–10000
- [ ] Reject `part_id < 1` or `part_id > 10000` → `400`
- [ ] Write chunk to temp area keyed by `(upload_id, part_id)`

**Verify:** `build/integration/dav_features/dav-v2.feature` — chunk PUT scenarios. Confirm invalid part ID rejected.

### 5.7 Chunked upload v2 — MOVE assembly (phase 3)
- [ ] `MOVE /dav/uploads/{userId}/{upload_id}/.file` with `Destination` header
- [ ] Optional `OC-Total-Length`: if present, validate sum of chunk sizes matches
- [ ] Assemble chunks in `part_id` order, write to `Destination`
- [ ] Optional `X-OC-MTime` / `X-OC-CTime` honored
- [ ] `201 Created` or `204 No Content`

**Verify:** full three-phase chunked v2 upload; confirm assembled file matches original. Test with wrong `OC-Total-Length` → `400`.

### 5.8 Chunked upload v2 — DELETE (abort)
- [ ] `DELETE /dav/uploads/{userId}/{upload_id}` removes all temp chunks and cache entry
- [ ] `204 No Content`

**Verify:** start upload, abort with DELETE, confirm temp files removed.

### 5.9 Bulk upload (`POST /dav/bulk`)
- [ ] Parse `multipart/related` body; per-part headers `X-File-Path`, `X-OC-MTime` / `X-File-MTime`, `Content-Length`
- [ ] Write each part to the correct path
- [ ] Response: JSON map of path → `{error, etag, fileid, permissions}`
- [ ] Partial failure: write what succeeded, include errors for failed parts in response

**Verify:** `build/integration/dav_features/dav-v2.feature` bulk upload scenario (if present); otherwise integration test: upload 5 files in one bulk request, confirm all present with correct etags.

### 5.10 ZIP/TAR folder download
- [ ] `GET /dav/files/{userId}/{folder}` with `Accept: application/zip` or `?accept=zip` → streamed ZIP; `Content-Disposition: attachment; filename="foldername.zip"`
- [ ] Same endpoint with `Accept: application/x-tar` or `?accept=tar` → streamed TAR archive (REQ §7.5)
- [ ] Root folder download named `download.zip` / `download.tar` respectively
- [ ] Optional `?files=["name1","name2"]` or `X-NC-Files` headers to filter children

**Verify:** `build/integration/files_features/download.feature` — ZIP and TAR download scenarios. Download a folder via each format, extract, verify contents match.

### 5.11 Checksum recalculation (`PATCH`) — STRETCH GOAL
> **Deferred:** `X-Recalculate-Hash` is an admin/integrity-check operation not triggered by any standard client sync or upload flow. Implement after all core upload flows are verified.

- [ ] `PATCH /dav/files/{userId}/{path}` with `X-Recalculate-Hash: {algorithm}`
- [ ] Recompute hash of stored file, update `oc_filecache.checksum`
- [ ] `204 No Content`, `OC-Checksum: {ALG}:{new_hash}` in response

**Verify:** `build/integration/files_features/checksums.feature` — PATCH recalculation scenario.
