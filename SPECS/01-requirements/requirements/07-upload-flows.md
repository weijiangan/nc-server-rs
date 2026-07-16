## 7. Upload Flows

### 7.1 Simple PUT upload

```
PUT /remote.php/webdav/{path}
Content-Type: application/octet-stream
Content-Length: {n}
X-OC-MTime: {unix_timestamp}          (optional)
X-OC-CTime: {unix_timestamp}          (optional)
OC-Checksum: MD5:{hash}              (optional; validated after write)
```

Response: `201 Created` (new file) or `204 No Content` (update).

Headers in response:
- `OC-FileId`, `ETag`, `OC-ETag`
- `X-OC-MTime: accepted` if mtime was set
- `X-OC-CTime: accepted` if ctime was set

`X-OC-MTime` / `X-OC-CTime` values are **sanitized** (`MtimeSanitizer`): they must be numeric (not hexadecimal) and `> 86400` (one day); an invalid value is rejected rather than silently honoured.

### 7.2 Chunked upload v1 (OC-Chunked)

Used by older desktop clients. Header `OC-Chunked: 1` present on each PUT.

Path convention: `{filename}-chunking-{transfer_id}-{total_chunks}-{chunk_index}`

Example:
```
PUT /remote.php/webdav/photo.jpg-chunking-1234567890-5-0
OC-Chunked: 1
Content-Length: {chunk_size}
```

On final chunk: assemble all parts, write to target `photo.jpg`, return `201 Created`.

### 7.3 Chunked upload v2 (TUS-style, requires distributed cache)

Three-phase protocol:

**Phase 1 – Create upload slot:**
```
MKCOL /dav/uploads/{userId}/{upload_id}
Destination: /dav/files/{userId}/{target_path}
```
Response: `201 Created`
Server calls `storage->startChunkedWrite(target_path)`, stores `(upload_id, target_path, chunk_upload_id)` in distributed cache (`memcache.distributed` required — Redis or Memcached).

**Phase 2 – Upload chunks:**
```
PUT /dav/uploads/{userId}/{upload_id}/{part_id}
Content-Length: {n}
```
`part_id` is a **numeric part index (1–10000)**, not a byte offset. Server validates `1 ≤ part_id ≤ 10000` and calls `storage->putChunkedWritePart($storagePath, $uploadId, $partId, $stream, $size)`.

**Phase 3 – Assemble:**
```
MOVE /dav/uploads/{userId}/{upload_id}/.file
Destination: /dav/files/{userId}/{target_path}
OC-Total-Length: {total_bytes}         (optional; validated if present)
X-OC-MTime: {unix_timestamp}           (optional)
X-OC-CTime: {unix_timestamp}           (optional)
```
Server calls `storage->completeChunkedWrite(...)`.
Response: `201 Created` (new) or `204 No Content` (replace).

**Abort:**
```
DELETE /dav/uploads/{userId}/{upload_id}
```
Calls `storage->cancelChunkedWrite(...)`. Cleans up cache entry.

**Prerequisite:** Chunked upload v2 requires a distributed cache configured (`memcache.distributed` set in config). If not available, server must gracefully fall back (pretend the endpoint doesn't exist / let client fall back to v1 or simple PUT).

### 7.4 Bulk upload (`POST /dav/bulk`)

`Content-Type: multipart/related; …`

Each part has per-part headers:
- `X-File-Path: /path/relative/to/user/root`
- `X-File-MTime` (preferred) or `X-OC-MTime: {timestamp}` — mtime, sanitized as in §7.1
- `Content-Length: {n}` — **required** (a missing length is `411 Length Required`)
- `X-File-MD5` and/or `OC-Checksum: {ALG}:{hash}` — the part content is **validated** against these; a mismatch fails that part (PHP `MultipartRequestParser::validateHash()`)

Response: JSON map of path → `{error, etag, fileid, permissions}` for each file.

If parsing fails mid-stream, respond `400 Bad Request` with partial results written so far.

### 7.5 ZIP/TAR folder download

```
GET /dav/files/{userId}/{folder_path}
Accept: application/zip
```
or `?accept=zip` query param. Also accepts `application/x-tar` or `?accept=tar`.

Optional filters:
- `?files=["name1","name2"]` — only include listed children
- `X-NC-Files: name1` (multiple headers) — same effect

Response: streamed archive with `Content-Disposition: attachment; filename="foldername.zip"`.

When downloading the root folder, the archive is named `download.zip`.

---

---

Prev: [`06-webdav-dav.md`](06-webdav-dav.md) · Up: [`README.md`](README.md) · Next: [`08-files-app-rest-endpoints.md`](08-files-app-rest-endpoints.md)
