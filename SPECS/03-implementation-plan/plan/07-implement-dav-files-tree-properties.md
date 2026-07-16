## 4) Implement DAV files tree + properties

`dav-server-rs` handles all RFC4918 protocol mechanics. Our work is making Nextcloud's storage satisfy its three traits:

### `DavFileSystem` trait — Nextcloud storage adapter
1. Implement `DavFileSystem` backed by the Nextcloud DB + object/local storage:
   - `read_dir`, `metadata`, `get_file`, `put_file`, `remove_file`, `remove_dir`, `create_dir`.
   - Map Nextcloud node IDs, permissions, and path resolution through the filecache table.
2. Implement `DavProp` via `DavFileSystem` to return Nextcloud's custom properties alongside standard ones:
   - `{oc:}id`, `{oc:}fileid`, `{oc:}permissions`, `{oc:}size`, `{oc:}owner-id`, `{oc:}checksums`, `{oc:}data-fingerprint`.
   - `{nc:}has-preview`, `{nc:}mount-type`, `{nc:}creation_time`, `{nc:}upload_time`, `{nc:}hidden`, `{nc:}download-url-expiration`, etc.
   - `{nc:}metadata_etag`: read from `oc_filecache_extended.metadata_etag`. **TODO:** The PHP reference implementation defines `METADATA_ETAG_PROPERTYNAME` in `FilesPlugin` but never wires it to a `$propFind->handle()` call, so it is never returned. Implement it correctly in Rust.
   - DAV quota properties (`{DAV:}quota-available-bytes` reports `-3` (`FileInfo::SPACE_UNLIMITED`) for unlimited quota; `{DAV:}quota-used-bytes`). Any negative free-space value skips the quota check and allows the write.
3. Implement `SEARCH` request handling via `SearchDAV` library / `FileSearchBackend`:
   - Scope: `Directory` nodes, typically `/dav/files/{userId}`.
   - Supported filters: name, MIME type, size, last-modified, tags, file metadata.
   - Response: `207 Multi-Status`.
5. Respond with required headers after writes:
   - `OC-FileId`, `OC-ETag` (mirror ETag), `X-OC-MTime: accepted` when mtime honored.

### `DavLockSystem` trait — fake locking for client compat
1. Implement `DavLockSystem` as an in-memory no-op lock store (matches `FakeLockerPlugin` semantics): respond to LOCK/UNLOCK so macOS Finder / OneNote / WebDAVFS can mount.

### File tree URLs
1. Mount adapter at `/dav/files/{userId}` for authenticated user trees.
2. Mount a separate adapter at `/dav/uploads/{userId}` for chunked upload assembly area.

### DAV compatibility plugins (required for client compat)

Implement the following plugins that parallel SabreDAV plugins used in production (see REQ §14):

| Plugin | Behavior |
| --- | --- |
| `AnonymousOptionsPlugin` | Handles unauthenticated `OPTIONS` and `HEAD` requests from **Microsoft Office** user-agents (empty `Authorization`). Sets up a fake tree and returns a valid OPTIONS response so Office can probe the endpoint without an auth pop-up. Not a general CORS handler. |
| `AppleQuirksPlugin` | Intended to fix a specific macOS DAV client issue: when a macOS agent sends a `{DAV:}principal-property-search` REPORT, force-set `applyToPrincipalCollectionSet = true`. **⚠ Known PHP bug:** `AppleQuirksPlugin::isMacOSUserAgent()` has its `str_starts_with` arguments reversed (`str_starts_with("macOS", $userAgent)` instead of `str_starts_with($userAgent, "macOS")`), so the UA check never matches and the plugin is effectively a no-op. Replicate this broken behavior (implement as a no-op) for compatibility. A correct implementation would break the intended principal search behaviour for macOS clients, but no clients currently rely on it since it has never worked. |
| `BlockLegacyClientPlugin` | Returns 403 for desktop sync clients below `minimum.supported.desktop.version` config value **or** above `maximum.supported.desktop.version` (both configurable; defaults `3.1.81` and `99.99.99`). |
| `FakeLockerPlugin` | Already covered by `DavLockSystem` trait |
| `DummyGetResponsePlugin` | Intercepts any `GET` request on the DAV tree (priority 200) and returns HTTP 200 with the plain-text body: `"This is the WebDAV interface. It can only be accessed by WebDAV clients such as the Nextcloud desktop sync client."` No debug-mode check. Prevents SabreDAV's built-in directory browser from being shown. |
| `RequestIdHeaderPlugin` | Inject `X-Request-Id` UUID on all responses |
| `UserIdHeaderPlugin` | Inject `X-Nextcloud-User-Id` on all authenticated responses |
| `CopyEtagHeaderPlugin` | Mirror `ETag` as `OC-ETag` on every response that has an ETag |
| `FilesDropPlugin` | Enforces upload-only restrictions on file-drop public shares (`/public.php/dav`). Allowed methods: `PUT`, `MKCOL`, and `MOVE` (the last only for chunked upload assembly). All other methods throw `MethodNotAllowed` (HTTP **405**). Also handles nickname headers, path rewriting, and conflict resolution for duplicate filenames. |

### GET checksum response + PATCH recalculation (REQ §13)

1. On file `GET` responses: include `OC-Checksum: {ALGORITHM}:{hash}` header if a checksum is stored in `oc_filecache.checksum`.
2. Implement `PATCH /{path}` with `X-Recalculate-Hash: {algorithm}` header: recompute the stored hash, update `oc_filecache.checksum`, and respond `204 No Content` with `OC-Checksum: {ALGORITHM}:{new_hash}`.

### Verification steps
Reuse existing integration suites — no new test infrastructure needed:
- `build/integration/dav_features/webdav-related.feature`
- `build/integration/dav_features/dav-v2.feature`
- `build/integration/dav_features/dav-v2-public.feature`
- `build/integration/dav_features/principal-property-search.feature`
- `build/integration/files_features/checksums.feature`
- `build/integration/files_features/metadata.feature`
- `build/integration/files_features/tags.feature`
- Cypress `cypress/e2e/files/*.cy.ts`

Compare PROPFIND response bodies (namespace + property presence) against PHP/SabreDAV baseline snapshots to confirm property parity.

---

Prev: [`06-implement-dav-service-routing-auth-stack.md`](06-implement-dav-service-routing-auth-stack.md) · Up: [`README.md`](README.md) · Next: [`08-implement-upload-flows-must-have-for-desktop-mobile-clients.md`](08-implement-upload-flows-must-have-for-desktop-mobile-clients.md)
