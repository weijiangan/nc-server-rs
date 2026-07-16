## WebDAV, CalDAV, CardDAV

### Service mapping

`remote.php` maps the first path segment:

- `webdav` -> `apps/dav/appinfo/v1/webdav.php`
- `dav` -> `apps/dav/appinfo/v2/remote.php`
- `caldav`/`calendar` -> `apps/dav/appinfo/v1/caldav.php`
- `carddav`/`contacts` -> `apps/dav/appinfo/v1/carddav.php`
- `files` -> `apps/dav/appinfo/v1/webdav.php`
- `direct` -> `apps/dav/appinfo/v2/direct.php`

`public.php` exposes public DAV routes:

- `webdav` -> `apps/dav/appinfo/v1/publicwebdav.php`
- `dav` -> `apps/dav/appinfo/v2/publicremote.php`

### DAV server expectations

- DAV is powered by SabreDAV (`apps/dav`).
- `apps/dav/appinfo/v2/remote.php` creates a Sabre server via `ServerFactory`.
- `apps/dav/appinfo/v2/publicremote.php` sets base URI to `/public.php/dav/files/{TOKEN}`
  and enforces non-GET restrictions for public shares.
- Output buffering is disabled and time limits are removed for DAV routes.
- Maintenance/upgrade mode for DAV returns 503 before server start.
- CSP header `Content-Security-Policy: default-src 'none';` is set for DAV routes.

### WebDAV tree URL structure

The primary DAV endpoint is `remote.php/dav` (or `/dav` when front controller is active).
It serves several subtrees:

| Path | Purpose |
| --- | --- |
| `/dav/files/{userId}/...` | User file tree (requires auth as that user or admin). |
| `/dav/uploads/{userId}/...` | Temporary chunked upload assembly area (v1 OC-Chunked and v2). |
| `/dav/bulk` | Bulk upload endpoint (`POST` with multipart body). |
| `/dav/calendars/{userId}/...` | CalDAV calendar collections. |
| `/dav/addressbooks/{userId}/...` | CardDAV addressbook collections. |
| `/dav/public-calendars/{token}/` | Public CalDAV calendar. |
| `/dav/system-calendars/...` | System calendars. |
| `/dav/principals/users/{userId}` | DAV principal for a user. |
| `/dav/principals/groups/{groupId}` | DAV principal for a group. |

Legacy aliases served by `remote.php/webdav` (v1) map directly into the user's file tree
without a `files/{userId}` prefix.  Desktop clients use `remote.php/dav/files/{userId}`.

### Nextcloud-specific WebDAV properties

The `FilesPlugin` (`apps/dav/lib/Connector/Sabre/FilesPlugin.php`) registers two XML
namespaces and exposes many custom DAV properties.  Desktop sync clients require these.

**Namespace `{http://owncloud.org/ns}` (prefix `oc`)**

| Property | Meaning |
| --- | --- |
| `{oc:}id` | Globally-unique file/folder ID: `{fileId}{instanceId}` (padded). |
| `{oc:}fileid` | Numeric file ID (same as `id` but without instance suffix). |
| `{oc:}permissions` | Permission string (e.g. `RGDNVW`): R=read, G=share, D=delete, N=rename, V=move, W=write, CK=create. |
| `{oc:}size` | Recursive size in bytes (directories include all children). |
| `{oc:}owner-id` | UID of the owning user. |
| `{oc:}owner-display-name` | Display name of the owning user. |
| `{oc:}downloadURL` | Temporary signed direct-download URL (if configured). |
| `{oc:}checksums` | Comma-separated checksum list (e.g. `SHA1:abc MD5:def`). |
| `{oc:}data-fingerprint` | Changes when server data changes (used to detect remote wipe). |

**Namespace `{http://nextcloud.org/ns}` (prefix `nc`)**

| Property | Meaning |
| --- | --- |
| `{nc:}has-preview` | Boolean; true if a preview is available. |
| `{nc:}mount-type` | Mount type string (e.g. `shared`, `external`, `group`). |
| `{nc:}is-mount-root` | Boolean; true if this node is the root of a mount point. |
| `{nc:}is-federated` | Boolean; true for federated shares. |
| `{nc:}metadata_etag` | ETag covering just metadata (tags, comments), not content. |
| `{nc:}upload_time` | Unix timestamp of when the file was first uploaded. |
| `{nc:}creation_time` | Unix timestamp of original creation. |
| `{nc:}hidden` | Boolean; whether this file/folder is hidden. |
| `{nc:}share-attributes` | JSON-encoded share attribute list. |
| `{nc:}note` | Share note. |
| `{nc:}hide-download` | Boolean; whether the download button is hidden for a share. |
| `{nc:}contained-folder-count` | Number of direct child folders. |
| `{nc:}contained-file-count` | Number of direct child files. |
| `{nc:}metadata-{key}` | Arbitrary file metadata (prefixed with `metadata-`). |

**Standard DAV quota properties** (provided by `QuotaPlugin`):

| Property | Meaning |
| --- | --- |
| `{DAV:}quota-available-bytes` | Remaining quota in bytes (-3 if unlimited). |
| `{DAV:}quota-used-bytes` | Bytes currently used. |

**Open Collaboration Services namespace `{http://open-collaboration-services.org/ns}`**:
- `{ocs:}share-permissions`: numeric share permission mask.

**Open Cloud Mesh namespace `{http://open-cloud-mesh.org/ns}`**:
- `{ocm:}share-permissions`: share permissions for federated shares.

All of the `{oc:}` and `{nc:}` properties above are **protected** (read-only via PROPPATCH)
except `{DAV:}getetag`, which is intentionally made writable.

### Chunked upload protocols

Nextcloud desktop clients use two chunked upload protocols.  Both are required.

#### v1 — OC-Chunked (legacy)

All requests go to the regular file path with extra headers and a naming convention.

1. Split the file into N chunks.  For each chunk `i` (0-indexed), PUT to:
   `remote.php/dav/files/{userId}/{filename}-chunking-{uploadId}-{N}-{i}`
   with header `OC-Chunked: 1`.
2. After all N chunks are uploaded, `MOVE` the assembled path to the final destination.
   The server detects the last chunk and auto-assembles.
3. The `X-OC-MTime` header on the final PUT/MOVE sets the file modification time.
4. On success, the server returns 201 with `OC-FileId` and `OC-ETag` headers.

Implementation in `apps/dav/lib/Upload/ChunkingPlugin.php`.

#### v2 — TUS-like / named upload folder

Upload folder lives under `/dav/uploads/{userId}/`.

1. `MKCOL /dav/uploads/{userId}/{uploadFolderName}` — creates the temporary upload folder.
   The `Destination` header **must be present at MKCOL time** pointing to the final target path
   (e.g. `/dav/files/{userId}/path/to/target`). `ChunkingV2Plugin` reads it here to initialize
   the chunked write and caches the target path in the distributed cache.
2. `PUT /dav/uploads/{userId}/{uploadFolderName}/{partId}` — upload each chunk. The resource
   name is a **numeric part ID between 1 and 10000** (not a byte offset). `ChunkingV2Plugin`
   validates `1 ≤ partId ≤ 10000`.
3. `MOVE /dav/uploads/{userId}/{uploadFolderName}/.file` to the final destination path, with:
   - `Destination: /dav/files/{userId}/path/to/target`
   - `OC-Total-Length: {total byte size}` — triggers assembly and validation.
   - `X-OC-MTime: {unix timestamp}` (optional).
4. Assembly uses `IChunkedFileWrite` or object-store multipart upload if available.
5. The `Destination` header drives the `ChunkingPlugin` `beforeMove` handler.

Implementation in `apps/dav/lib/Upload/ChunkingV2Plugin.php`.

### Bulk upload

`POST /dav/bulk` accepts a `multipart/related` request where each part is a file to write.

Per-part headers:
- `X-File-Path: /relative/path/in/user/folder` — destination path.
- `X-OC-MTime: {unix timestamp}` (or `X-File-MTime` for legacy clients).
- `Content-Length` of the part body.

Response: JSON object mapping each path to `{ error, etag, fileid, permissions }`.

Implementation in `apps/dav/lib/BulkUpload/BulkUploadPlugin.php`.

### DAV response headers

These headers are sent on successful write operations and must be included:

| Header | Set by | Meaning |
| --- | --- | --- |
| `OC-FileId` | `FilesPlugin.sendFileIdHeader` | Global file ID (after PUT, COPY, MOVE). |
| `OC-ETag` | `CopyEtagHeaderPlugin` | Mirrors the `ETag` header on every response that has one. |
| `X-OC-MTime: accepted` | `File.put` / `ChunkingV2Plugin` | Echoed when `X-OC-MTime` was honored. |
| `X-Request-ID` | `RequestIdHeaderPlugin` | Unique request ID for tracing. |
| `X-Nextcloud-User-Id` | `UserIdHeaderPlugin` | UID of the authenticated user. |

### DAV SEARCH method

The `SearchPlugin` (from `SearchDAV` library) is registered on the `/dav` endpoint.
It handles `SEARCH` requests using a DAV-specific XML query body.

- Scope: any `Directory` node (typically `/dav/files/{userId}`).
- Supported filter properties include name, MIME type, size, last-modified, tags, and
  file metadata fields.
- Returns `207 Multi-Status` with matching nodes and requested properties.

Implementation in `apps/dav/lib/Files/FileSearchBackend.php`.

### WebDAV delta sync (RFC 6578)

`Sabre\DAV\Sync\Plugin` is registered on the DAV server, but it only activates for
collections that implement `ISyncCollection` — i.e. the **CalDAV and CardDAV** trees.
The **files** connector does **not** implement `getChanges()`/`ISyncCollection`, so
`sync-collection` REPORT and the `{DAV:}sync-token` property are **not** available on
file collections. The Nextcloud desktop client syncs files via ETag propagation on
parent folders, not RFC 6578.

### Zip folder download

`ZipFolderPlugin` (`apps/dav/lib/Connector/Sabre/ZipFolderPlugin.php`) intercepts
`GET` requests on directories that include `?accept=zip`.  It streams a ZIP archive of
the folder contents with `Content-Type: application/zip`.

### DAV client compatibility plugins

These plugins must be present to avoid breaking standard WebDAV clients:

| Plugin | Behavior |
| --- | --- |
| `AnonymousOptionsPlugin` | Handles unauthenticated `OPTIONS` and `HEAD` requests from **Microsoft Office** user-agents (empty or bare-Bearer `Authorization`). Returns a valid OPTIONS response without requiring auth. Not a general CORS handler. |
| `AppleQuirksPlugin` | Fixes macOS DAV client quirk: forces `applyToPrincipalCollectionSet = true` on `{DAV:}principal-property-search` REPORT requests from macOS agents so principal searches return correct results. Does not strip headers. |
| `BlockLegacyClientPlugin` | Returns 403 for desktop sync clients below `minimum.supported.desktop.version` **or** above `maximum.supported.desktop.version` (config keys, defaults `3.1.81` / `99.99.99`). |
| `FakeLockerPlugin` | Emulates `DAV: 2` locking for clients (e.g. OneNote, macOS WebDAVFS) that require Class 2 WebDAV.  Activated by user-agent matching. |
| `DummyGetResponsePlugin` | Intercepts any `GET` on the DAV tree (priority 200) and returns HTTP 200 plain-text body: `"This is the WebDAV interface…"`. No debug-mode condition. Prevents SabreDAV's built-in directory browser from responding. |

### Auth and tokens

DAV uses:

- Session/basic auth (`OCA\DAV\Connector\Sabre\Auth`).
  - Sets session key `AUTHENTICATED_TO_DAV_BACKEND` to the user UID so that
    subsequent requests on the same session do not repeat credential validation.
  - Two-factor auth is enforced: users with pending 2FA cannot authenticate via DAV.
  - Brute-force throttling applies on failed basic auth attempts.
- Bearer auth plugin for OAuth tokens (`BearerAuth`).
  - Calls `IUserSession::tryTokenLogin` to validate the Bearer token.
  - On failure, returns 401 without a `WWW-Authenticate` challenge (except for legacy
    ownCloud clients detected by the `mirall` user-agent string when enabled via config).
- Public share auth for `/public.php` (`PublicAuth`).

Clients expect Bearer tokens to work for DAV and OCS routes, and to honor `OCS-APIREQUEST`.

---

Prev: [`06-login-flows-and-oauth2.md`](06-login-flows-and-oauth2.md) · Up: [`README.md`](README.md) · Next: [`08-security-and-request-validation.md`](08-security-and-request-validation.md)
