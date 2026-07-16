## 5) Implement upload flows (must-have for desktop/mobile clients)

### Coding steps
1. Implement direct PUT upload with checksum and mtime handling.
2. ~~Implement chunked upload v1 (`OC-Chunked` flow).~~ **Cut:** OC-Chunked was never a web or mobile protocol and is not used by desktop sync clients ≥3.0 (2020). Requests with `OC-Chunked: 1` return `501 Not Implemented`.
3. Implement chunked upload v2 (`MKCOL` + chunk PUT + final `MOVE` with `OC-Total-Length`).
   - Note: PUT chunk path uses a **numeric part ID (1–10000)**, not a byte offset — matches `ChunkingV2Plugin` which validates `$partId >= 1 && $partId <= 10000`.
   - The `Destination` header is **required at MKCOL time** (not only on the final MOVE).
4. Implement bulk upload endpoint (`POST /dav/bulk`).
5. Implement folder ZIP download behavior (`?accept=zip`).
6. Implement filename validation before any write (PUT, MKCOL, MOVE, COPY target):
   - Check against `forbidden_filenames`, `forbidden_filename_basenames`, `forbidden_filename_characters`, `forbidden_filename_extensions` from `oc_appconfig`.
   - On violation: `422 Unprocessable Entity`.
7. Implement quota enforcement before all writes:
   - Compare `max(Content-Length, X-Expected-Entity-Length, OC-Total-Length)` against `free_space()`.
   - Skip quota check (allow write) when `free_space()` returns any **negative** value (`SPACE_NOT_COMPUTED = -1`, `SPACE_UNKNOWN = -2`, `SPACE_UNLIMITED = -3`, or `false`). REQ.md mentions only `SPACE_UNKNOWN` but the actual `QuotaPlugin.checkQuota()` treats all negative free-space as "allow".
   - Quota exceeded: `507 Insufficient Storage`.

### Verification steps
1. Reuse existing integration suites:
	- `build/integration/files_features/checksums.feature`
	- `build/integration/files_features/download.feature`
	- `build/integration/dav_features/dav-v2.feature`
	- `build/integration/dav_features/webdav-related.feature`
2. Add focused Rust-side tests for chunk assembly edge cases:
	- missing chunk
	- wrong `OC-Total-Length`
	- interrupted move/retry
	- concurrent uploads to same target path.

---

---

Prev: [`07-implement-dav-files-tree-properties.md`](07-implement-dav-files-tree-properties.md) · Up: [`README.md`](README.md) · Next: [`09-files-app-http-apis-stretch-goal.md`](09-files-app-http-apis-stretch-goal.md)
