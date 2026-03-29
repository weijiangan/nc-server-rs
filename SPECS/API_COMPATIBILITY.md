# Nextcloud Core API Compatibility Notes

This document captures the minimum behavior needed to provide an API-compatible
core reimplementation (e.g. Rust) without breaking Nextcloud clients. It focuses
on public HTTP entry points, response envelopes, and cross-cutting behaviors that
clients and apps depend on. A full reimplementation requires matching the APIs
registered by core apps (see “Core app API surfaces” below).

## Scope

- Core HTTP entry points and routing behavior.
- OCS API formats, status codes, and headers.
- WebDAV/CalDAV/CardDAV endpoints and auth expectations.
- Compatibility considerations for apps (PHP vs AppAPI).

Out of scope (for a core+files focused reimplementation): CalDAV/CardDAV internals,
internal PHP app logic, UI rendering details, and optional notification/push systems.

## Primary entry points

| Endpoint | Purpose | Notes |
| --- | --- | --- |
| `/index.php` | Front controller | Calls `OC::handleRequest()` (lib/base.php). |
| `/remote.php/{service}/...` | DAV and remote services | Service mapping in `remote.php`. |
| `/public.php/{service}/...` | Public DAV | Public shares and files drop. |
| `/ocs/v1.php` | OCS v1 API | XML default, 200 for most statuses. |
| `/ocs/v2.php` | OCS v2 API | XML default, status codes preserved. |
| `/ocs-provider/index.php` | OCS providers list | JSON list of available providers. |
| `/status.php` | Instance status | JSON, CORS `Access-Control-Allow-Origin: *`. |

## Request lifecycle and global behavior

- Initialization and config loading happens in `lib/base.php` (`OC::handleRequest()`).
- `OC::checkInstalled()` redirects to `/index.php` when not installed.
- `OC::checkMaintenanceMode()` sends HTTP 503, header `X-Nextcloud-Maintenance-Mode: 1`,
  and `Retry-After: 120` for most routes.
- `OC::$WEBROOT` is derived from `SCRIPT_NAME`/`REQUEST_URI` and influences URL generation.
- `apps_paths` config affects app lookup and autoloading; missing apps folder is fatal.
- `htaccess.IgnoreFrontController` or `front_controller_active=true` removes `/index.php`
  from generated URLs, but clients still need to accept `/index.php` paths.
- `index.php` emits JSON for API clients when `Accept` does not include `html`
  (see login and brute-force error handling in `index.php`).

## Routing and URL structure

### Route registration

- App routes are defined in `apps/*/appinfo/routes.php` and registered by `OC\Route\Router`.
- `RouteConfig` and `RouteParser` derive the actual URL prefix:
  - Default web routes for most apps use `/apps/{app}`.
  - Apps in the root whitelist (`cloud_federation_api`, `core`, `files_sharing`,
    `files`, `profile`, `settings`, `spreed`) can mount at `/`.
  - OCS routes are always whitelisted for root paths and use the OCS route collection.
- OCS routes are registered in a separate collection with `/ocsapp` prefix.
- `ocs/v1.php` and `ocs/v2.php` dispatch to routes by matching
  `'/ocsapp' . rawPathInfo` (see `ocs/v1.php`).

### Entry point URL prefixes

| Prefix | Notes |
| --- | --- |
| `/index.php` | May be omitted when front controller is active. |
| `/ocs/v1.php` and `/ocs/v2.php` | OCS dispatchers (map to `/ocsapp` routes). |
| `/ocs` | Router collection prefix for legacy OCS routes. |
| `/apps/{app}` | Default app web routes. |
| `/remote.php` | DAV and remote services. |
| `/public.php` | Public DAV and sharing. |

## OCS API compatibility

### Routing and format

- OCS endpoints are dispatched by `ocs/v1.php` and `ocs/v2.php`.
- Format defaults to XML (`format` query param or `Accept` header).
- `OCS-APIREQUEST: true` is required for CSRF bypass on OCS routes (see Security section).
- Content types:
  - XML: `text/xml; charset=UTF-8` (from `OCS\ApiHelper`).
  - JSON: `application/json; charset=utf-8`.

### Response envelope

OCS responses are wrapped as:

```json
{
  "ocs": {
    "meta": {
      "status": "ok|failure",
      "statuscode": 100|...,
      "message": "OK|error message",
      "totalitems": "...",
      "itemsperpage": "..."
    },
    "data": { ... }
  }
}
```

XML uses the same structure (`<ocs><meta>...</meta><data>...</data></ocs>`).

### Status code mapping

- OCS v1 (`OCS\V1Response`):
  - HTTP 200 for most responses; HTTP 401 for `RESPOND_UNAUTHORISED`.
  - OCS status `100` means OK; otherwise the OCS status equals the response status.
- OCS v2 (`OCS\V2Response`):
  - HTTP status equals the response status with mappings:
    - `RESPOND_UNAUTHORISED` -> 401
    - `RESPOND_NOT_FOUND` -> 404
    - `RESPOND_SERVER_ERROR` or `RESPOND_UNKNOWN_ERROR` -> 500
    - status <200 or >600 -> 400

### OCS format detection

- `format` query parameter takes precedence: `?format=json` or `?format=xml`.
- If absent, the `Accept` header is inspected; `application/json` selects JSON.
- Default is XML when neither is specified.
- `ocs/v1.php` vs `ocs/v2.php` is determined by the script name (see `OCS\ApiHelper::isV2`).

### Core OCS endpoints

These are served by `core/Controller/OCSController.php`:

- `GET /ocs/v1.php/config` and `/ocs/v2.php/config`
  - Returns `{ version: "1.7", website: "Nextcloud", host, contact, ssl }`.
- `GET /ocs/v1.php/cloud/capabilities` and `/ocs/v2.php/cloud/capabilities`
  - Returns `version` fields and `capabilities`.
  - Two separate capability providers contribute to `core`:
    - `OC\OCS\CoreCapabilities` (`lib/private/OCS/CoreCapabilities.php`):
      `core.pollinterval` (default 60), `core.webdav-root` (default `remote.php/webdav`),
      `core.reference-api: true`, `core.reference-regex`, `core.mod-rewrite-working`.
    - `OC\Core\AppInfo\Capabilities` (`core/AppInfo/Capabilities.php`):
      per-user fields `core.user.language`, `core.user.locale`, `core.user.timezone`
      and `core.can-create-app-token` (only when a user is authenticated).
  - Unauthenticated requests receive only `IPublicCapability` results.
  - The ETag of the capabilities response is set to `md5(json_encode($result))`.
- `POST /ocs/v1.php/person/check` and `/ocs/v2.php/person/check`
  - Uses `login`/`password`, returns status 200 with person data, or OCS status 101/102.
  - Protected by brute-force throttling (`@BruteForceProtection(action: 'login')`).
- `GET /ocs/v1.php/identityproof/key/{cloudId}` and v2 equivalent
  - Returns public key or 404.

### OCS auth headers

When responding with `RESPOND_UNAUTHORISED`, `OCS\ApiHelper` sets:

- `WWW-Authenticate: Basic realm="Authorisation Required"` (or DummyBasic for XHR).
- When `X-Requested-With: XMLHttpRequest` is set, value becomes `DummyBasic realm="Authorisation Required"`.

### Rate limiting (brute force)

- When `MaxDelayReached` is thrown, both `index.php` and `ocs/v1.php` return HTTP 429.
- `index.php`: if `Accept` does not include `html`, returns JSON `{"message": "..."}` with 429.
- `ocs/v1.php` / `ocs/v2.php`: returns OCS envelope with `Http::STATUS_TOO_MANY_REQUESTS`.
- The throttler adds a `Retry-After` header indicating when the client may retry.

## Login flows and OAuth2

### Login flow v1 (app password)

Routes in `core/Controller/ClientFlowLoginController.php`:

- `GET /login/flow` and `GET /login/flow/grant` render login flow pages.
- `POST /login/flow` exchanges state token for an app password.
- Requires `OCS-APIREQUEST: true` header or valid OAuth client identifier for access.
- State token stored in session (`client.flow.state.token`) and compared via `hash_equals`.

### Login flow v2 (token-based)

Routes in `core/Controller/ClientFlowLoginV2Controller.php`:

- `POST /login/v2/poll` returns JSON credentials when flow completes.
- `GET /login/v2/flow/{token}` and `GET /login/v2/flow` manage the flow state.
- `GET /login/v2/grant` and `POST /login/v2/apptoken` finalize the flow.
- Uses session keys `client.flow.v2.login.token` and `client.flow.v2.state.token`.

### OAuth2 token endpoint

Routes in `apps/oauth2/appinfo/routes.php`:

- `POST /apps/oauth2/api/v1/token` returns bearer tokens used by OCS and DAV.
- `GET /apps/oauth2/authorize` initiates OAuth2 auth picker flow.

Bearer tokens must be accepted by OCS (CSRF bypass) and DAV (BearerAuth).

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

`Sabre\DAV\Sync\Plugin` is registered on all DAV trees.  Clients may use
`sync-collection` REPORT requests with a `sync-token` to fetch only changed resources
since their last sync.  The `{DAV:}sync-token` property is available on collections.

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

## Security and request validation

`SecurityMiddleware` enforces:

- Login required unless `PublicPage`.
- CSRF check except for `NoCSRFRequired` or OCS with `OCS-APIREQUEST: true` or
  `Authorization: Bearer ...`.
- Strict cookie checks unless `NoCSRFRequired` is set.
- Admin/sub-admin access for admin routes.
- IP-based admin access restrictions (`IRemoteAddress::allowsAdminActions()`).
- ExApp-required routes (`ExAppRequired` attribute) check the session `app_api` flag.

Rust reimplementation must keep the same semantics for these attributes, especially
the OCS CSRF exceptions and Bearer token bypass.

### CSRF and session cookies

Nextcloud uses a double-submit cookie + server-side token pattern:

- On first page load, a `requesttoken` is embedded in initial state (HTML or API).
- Subsequent mutating requests (non-GET, non-OCS-with-APIREQUEST) must include the
  token via the `requesttoken` form field or `OCS-APIREQUEST: true` header.
- Three cookies are set:
  - `nc_session_id` — the PHP session identifier.
  - `nc_token` — the persistent remember-me token (also triggers the SameSite check when present).
  - `nc_sameSiteCookielax` — dummy cookie with `SameSite=lax` (**lowercase** suffix in actual cookie name).
  - `nc_sameSiteCookiestrict` — dummy cookie with `SameSite=strict` (**lowercase** suffix in actual cookie name).
  - On HTTPS with path `/`, the `__Host-` prefix is prepended to the SameSite cookie names.
- The strict cookie check (`passesStrictCookieCheck`) verifies both `nc_sameSiteCookiestrict=true` and `nc_sameSiteCookielax=true` are present, providing CSRF protection for requests that carry cookies.
- Cookie check is only triggered when the request includes `session_name()` (PHP session cookie) or `nc_token`. Requests with neither cookie bypass the check entirely.
- `SameSiteCookieMiddleware` only applies to requests routed through `index.php`.
- OCS routes bypass the strict cookie check if `OCS-APIREQUEST: true` is present.
- `Authorization: Bearer ...` also bypasses CSRF checks (token-authenticated requests).

## Public status endpoint

`/status.php` returns JSON:

```json
{
  "installed": true|false,
  "maintenance": true|false,
  "needsDbUpgrade": true|false,
  "version": "x.y.z",
  "versionstring": "x.y.z",
  "edition": "",
  "productname": "Nextcloud",
  "extendedSupport": true|false
}
```

Headers: `Access-Control-Allow-Origin: *`, `Content-Type: application/json`.

## Well-known endpoints

`core/Controller/WellKnownController.php` handles:

- `GET /.well-known/{service}` with header `X-NEXTCLOUD-WELL-KNOWN: 1`.
- Returns `404` with `{"message":"{service} not supported"}` when unknown.
- Setup checks expect:
  - `/\.well-known/webfinger` -> 200/400/404
  - `/\.well-known/nodeinfo` -> 200/404
  - `PROPFIND /.well-known/caldav` -> 207
  - `PROPFIND /.well-known/carddav` -> 207

These should map to DAV roots for CalDAV/CardDAV.

## Files app REST API

The `files` app exposes HTTP endpoints in addition to the DAV tree.  Mobile and web
clients use these for thumbnails, recent-file lists, and configuration.

### Thumbnail endpoint

`GET /apps/files/api/v1/thumbnail/{width}/{height}/{path}`

- `path` matches `.+` and is a URL-encoded file path relative to the user's root.
- Returns the image scaled to `{width}x{height}` pixels.
- Served by `Api#getThumbnail` in `apps/files/lib/Controller/ApiController.php`.

### Recent files

`GET /apps/files/api/v1/recent/` — returns recently modified files as JSON.

### Storage statistics

`GET /apps/files/api/v1/stats` — returns used/free space for the user's storage.

### View and display config

- `GET/PUT /apps/files/api/v1/views/{view}/{key}` — per-view display preferences.
- `GET/PUT /apps/files/api/v1/configs` — global files app settings.
- `POST /apps/files/api/v1/showhidden` — toggle hidden-file display.
- `POST /apps/files/api/v1/showgridview` / `GET /apps/files/api/v1/showgridview`.

### OCS endpoints in the files app

Prefixed at `/ocs/v2.php/apps/files/`:

- `GET /api/v1/directEditing` — list available direct-editing handlers.
- `POST /api/v1/directEditing/open` — open a file in a direct-editing session.
- `POST /api/v1/directEditing/create` — create a new file via direct editing.
- `GET /api/v1/templates` — list file templates.
- `POST /api/v1/templates/create` — create a file from a template.
- `POST /api/v1/transferownership` — initiate ownership transfer.
- `POST /api/v1/openlocaleditor` — create a token for opening a file in a local editor.
- `POST /api/v1/openlocaleditor/{token}` — validate a local editor token.

Full route list in `apps/files/appinfo/routes.php`.

## Public sharing endpoints

The `files_sharing` app registers several public-facing routes (no authentication needed):

| Route | Purpose |
| --- | --- |
| `GET /s/{token}` | Render public share page. |
| `GET /s/{token}/authenticate/{redirect}` | Show password prompt. |
| `POST /s/{token}/authenticate/{redirect}` | Submit share password. |
| `GET /s/{token}/download/{filename}` | Direct download of shared file. |
| `GET /s/{token}/preview` | Preview image for a public share. |
| `GET /publicpreview/{token}` | Alternative preview URL. |
| `POST /shareinfo` | Returns share metadata for a given token. |

Public DAV for share tokens is at `/public.php/dav` (served by `publicremote.php`).
Files-drop shares (upload-only) are enforced by `FilesDropPlugin` on that endpoint.



To be fully API compatible, you must implement routes from the shipped core apps
listed below. Each app’s `appinfo/routes.php` is the authoritative route list.

| App | Key API surfaces | Route file |
| --- | --- | --- |
| `provisioning_api` | Users, groups, apps, app config, user prefs | `apps/provisioning_api/appinfo/routes.php` |
| `files_sharing` | OCS share API, sharees, remote shares, public shares | `apps/files_sharing/appinfo/routes.php` |
| `files` | Files app APIs, thumbnails, recent, templates, direct editing | `apps/files/appinfo/routes.php` |
| `dav` | DAV public routes, direct endpoints, upcoming events | `apps/dav/appinfo/routes.php` |
| `oauth2` | OAuth2 tokens and authorize redirect | `apps/oauth2/appinfo/routes.php` |
| `federation` | Shared-secret endpoints | `apps/federation/appinfo/routes.php` |
| `cloud_federation_api` | OCM share requests | `apps/cloud_federation_api/appinfo/routes.php` |
| `federatedfilesharing` | Federated share OCS endpoints | `apps/federatedfilesharing/appinfo/routes.php` |
| `comments` | Notifications view | `apps/comments/appinfo/routes.php` |
| `systemtags` | Tag usage | `apps/systemtags/appinfo/routes.php` |
| `files_versions` | Previews, download/rollback scripts | `apps/files_versions/appinfo/routes.php` |
| `files_trashbin` | Previews | `apps/files_trashbin/appinfo/routes.php` |

For complete API coverage, parse every `apps/*/appinfo/routes.php` and add support
for the declared routes and controller behaviors. Many routes depend on response
schemas defined in each app’s `ResponseDefinitions` class (e.g. `files_sharing`).

## App compatibility considerations

### PHP apps (traditional)

Existing apps are PHP and rely on:

- OCP interfaces (service container, request, user/session, files, config).
- OCS routing and response envelopes.
- App loading lifecycle (`OC_App::loadApps`, appinfo/routes).

To run existing PHP apps without rewriting:

1. Provide a PHP execution environment with compatible OCP API bindings, or
2. Implement an AppAPI bridge (external apps) and migrate apps to it.

### AppAPI (external apps)

The `app_api` stubs show a pattern where external apps are called with signed
headers and validated via `AppAPIService::validateExAppRequestToNC`. If AppAPI
compatibility is desired, the Rust core should expose the same auth header
validation and `ExAppRequired` semantics (`SecurityMiddleware`).

## Configuration values influencing API behavior

Key values (from `config/config.sample.php` and `CoreCapabilities`):

- `trusted_domains`, `overwritewebroot`, `htaccess.IgnoreFrontController`.
- `maintenance`, `installed`, `version`, `db*` settings.
- `pollinterval`, `webdav-root` (used in capabilities).

## Reference implementation locations

- Entry points: `index.php`, `remote.php`, `public.php`, `ocs/v1.php`, `ocs/v2.php`, `status.php`.
- OCS responses: `lib/private/AppFramework/OCS/*Response.php`, `lib/private/OCS/ApiHelper.php`.
- OCS capabilities: `lib/private/OCS/CoreCapabilities.php`, `core/AppInfo/Capabilities.php`.
- Security: `lib/private/AppFramework/Middleware/Security/SecurityMiddleware.php`.
- DAV servers: `apps/dav/appinfo/v1/*`, `apps/dav/appinfo/v2/*`, `apps/dav/lib/Server.php`.
- DAV file properties: `apps/dav/lib/Connector/Sabre/FilesPlugin.php`.
- Chunked upload v1: `apps/dav/lib/Upload/ChunkingPlugin.php`.
- Chunked upload v2: `apps/dav/lib/Upload/ChunkingV2Plugin.php`.
- Bulk upload: `apps/dav/lib/BulkUpload/BulkUploadPlugin.php`.
- DAV response headers: `apps/dav/lib/Connector/Sabre/CopyEtagHeaderPlugin.php`, `RequestIdHeaderPlugin.php`, `UserIdHeaderPlugin.php`.
- File search: `apps/dav/lib/Files/FileSearchBackend.php`.
- Quota enforcement: `apps/dav/lib/Connector/Sabre/QuotaPlugin.php`.
- Auth (DAV basic): `apps/dav/lib/Connector/Sabre/Auth.php`.
- Auth (DAV bearer): `apps/dav/lib/Connector/Sabre/BearerAuth.php`.
- Client quirks: `apps/dav/lib/Connector/Sabre/AnonymousOptionsPlugin.php`, `AppleQuirksPlugin.php`, `BlockLegacyClientPlugin.php`, `FakeLockerPlugin.php`.
- Files app REST: `apps/files/lib/Controller/ApiController.php`.
- Public sharing: `apps/files_sharing/lib/Controller/ShareController.php`.
