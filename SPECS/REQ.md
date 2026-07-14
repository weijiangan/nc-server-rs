# Requirements: Nextcloud Core + Files in Rust (API-Compatible)

This document captures the detailed requirements for reimplementing Nextcloud's **core** and **files** subsystems in Rust such that all existing clients (desktop sync, iOS, Android, WebDAV mounts, web UI) continue to work unchanged. PHP apps remain supported by forwarding requests to a PHP-FPM backend.

### ⚠ Requirement Gap: Trash Bin on DELETE

The `files_trashbin` app was categorised as "out of scope / delegated to PHP-FPM" (§1, §10.5) and its DAV endpoint (`/remote.php/dav/trashbin/…`) was routed to PHP-FPM (§6.1). This correctly covers the *read-side* (listing and restoring deleted files), but **misses the write-side**: the trash bin is not a self-contained app — it is a storage-wrapper that intercepts every `unlink()` call across the filesystem. The act of moving a file to trash happens during `DELETE /remote.php/dav/files/{userId}/…` — the Rust-native handler — not on the trashbin endpoint. By the time a request reaches `/dav/trashbin/…`, the file is already expected to be in the trash.

This gap was introduced because the scope boundaries were drawn along app/endpoint lines (the trashbin *app* and its *DAV subtree* are PHP-FPM), without tracing the cross-cutting dataflow: the write path runs through the files endpoint, not the trashbin endpoint. The `oc_files_trash` table and the `files_trashbin/files/` storage layout were also absent from the database schema (§9).

See §6.7 and §9.7 for the corrected requirements.

---

## 1. Scope

### In scope (native Rust)
- All HTTP entry-point routing (`index.php`, `remote.php`, `public.php`, `ocs/v1.php`, `ocs/v2.php`, `status.php`)
- OCS API envelope, format negotiation, auth signalling (`/ocs/v1.php/…`, `/ocs/v2.php/…`)
- Core OCS endpoints: `/cloud/capabilities`, `/ocs/v1.php/config`, `/person/check`, `/identityproof/key/{cloudId}`
- Authentication and session management (Basic, Bearer/token, CSRF logic, brute-force throttling)
- Full WebDAV implementation for `/remote.php/webdav`, `/remote.php/dav`, `/dav/files/{userId}`, `/dav/uploads/{userId}`, and public DAV at `/public.php/webdav` and `/public.php/dav`
- All DAV-related HTTP methods: PROPFIND, PROPPATCH, MKCOL, GET, PUT, DELETE, COPY, MOVE, LOCK, UNLOCK, PATCH, POST (bulk)
- Nextcloud-specific DAV properties (OC and NC namespaces)
- Chunked upload v1 (OC-Chunked header), chunked upload v2 (MKCOL + PUT + MOVE), bulk upload (`POST /dav/bulk`)
- ZIP/TAR folder download (`GET` on collection with `Accept: application/zip` or `?accept=zip`)
- Files app REST endpoints under `/apps/files/api/v1/…`
- Files app OCS endpoints (`/ocs/…/apps/files/api/v1/…`)
- Database ownership: migrations for all tables required by core + files, support for fresh installs and existing Nextcloud DBs
- Maintenance mode handling and `/status.php`
- Quota enforcement
- Brute-force protection (`oc_bruteforce_attempts`)
- In-process caching layer for hot paths

### Out of scope (delegated to PHP-FPM)
- All other apps: `files_sharing`, `provisioning_api`, `comments`, `systemtags`, `federation`, `federatedfilesharing`, `dav` CalDAV/CardDAV stacks, `oauth2`, `user_ldap`, `settings`, etc.
- The Nextcloud web UI HTML rendering (served from PHP-FPM via the index route)
- Two-factor authentication challenge pages (PHP-FPM renders these; Rust enforces 2FA status check)

---

## 2. HTTP Entry Points

### 2.1 Routes Rust must serve natively

| URL pattern | Handler |
|---|---|
| `GET /status.php` | Status JSON |
| `GET /heartbeat` | 200 OK |
| `GET /index.php` (and clean URL equivalents) | PHP-FPM fallback (or page not found for API-only mode) |
| `/ocs/v1.php/…` | OCS v1 |
| `/ocs/v2.php/…` | OCS v2 |
| `GET /ocs-provider/index.php` | OCS provider discovery (JSON list of available providers) |
| `/remote.php/{service}/…` | DAV service dispatch |
| `/public.php/{service}/…` | Public DAV dispatch |
| `/apps/files/…` | Files app (mix: REST native + PHP-FPM) |
| `GET /.well-known/{service}` | Well-known endpoints (webfinger, nodeinfo; DAV well-known redirects to CalDAV/CardDAV) |
| `GET /login/flow`, `POST /login/flow`, `GET /login/flow/grant` | Login flow v1 (app password generation) — PHP-FPM |
| `POST /login/v2/poll`, `GET /login/v2/flow/{token}`, `GET /login/v2/grant`, `POST /login/v2/apptoken` | Login flow v2 (token-based) — PHP-FPM |

### 2.2 `remote.php` service map

```
webdav  → dav/files/{userId}      (authenticated WebDAV v1 root)
files   → dav/files/{userId}      (alias)
dav     → DAV v2 tree              (authenticated, full path resolution)
caldav  → PHP-FPM (dav app)
calendar→ PHP-FPM (dav app)
carddav → PHP-FPM (dav app)
contacts→ PHP-FPM (dav app)
direct  → PHP-FPM (dav app)
```

### 2.3 `public.php` service map

```
webdav  → public WebDAV (public-share auth flow)
dav     → public DAV v2 (public-share auth flow)
```

### 2.4 Required response headers on every DAV endpoint

```
Content-Security-Policy: default-src 'none';
```

### 2.5 Maintenance mode

When `maintenance = true` in config, all non-`/status.php` endpoints must return:
- HTTP 503
- `X-Nextcloud-Maintenance-Mode: 1`
- `Retry-After: 120`
- Body as OCS error envelope (for OCS endpoints) or plain text (for DAV)

When schema needs upgrade (`Util::needUpgrade`), DAV endpoints return HTTP 503 immediately.

---

## 3. `/status.php`

Returns `Content-Type: application/json`, `Access-Control-Allow-Origin: *`, and:

```json
{
  "installed": true,
  "maintenance": false,
  "needsDbUpgrade": false,
  "version": "30.0.2.1",
  "versionstring": "30.0.2",
  "edition": "",
  "productname": "Nextcloud",
  "extendedSupport": false
}
```

All values read from `config/config.php` (system config) or DB (`oc_appconfig`). **`installed` and `maintenance` come from `config.php` via `SystemConfig::getValue()` — not from `oc_appconfig`.** Version strings (`oc_version`, `versionstring`) come from `oc_appconfig` under `core`.

---

## 4. Authentication

### 4.1 Authentication methods

| Method | Trigger | Notes |
|---|---|---|
| Basic (password) | `Authorization: Basic …` header | Password or app token |
| Bearer token | `Authorization: Bearer …` header | App token / OAuth2 token |
| Session cookie | `{instanceid}` cookie (PHP session) + SameSite guard cookies | Web browser flow (see §15.2 for cookie names). The PHP session cookie is named after `config.php`'s `instanceid` value (e.g., `oc1a2b3c4d5e`), set via `session_name(OC_Util::getInstanceId())` in `lib/base.php:437,447`. **Not** `nc_session_id` — that is a separate remember-me cookie. |
| Remember-me cookies | `nc_token` + `nc_username` + `nc_session_id` cookies | Used by `loginWithCookie()` when the PHP session has expired but remember-me cookies remain. All three must be present. `nc_token` is validated against `oc_preferences` `login_token` entries, rotated on each use. `nc_session_id` stores the old `session_id()` for `renewSessionToken()`. |
| Password-less token | Basic header, token has `passwordless = true` | No password login permitted |

### 4.2 Token storage (`oc_authtoken`)

Each app password / device token is a row in `oc_authtoken` with:
- `uid` (owner user)
- `login_name`
- `password` (encrypted password for token refresh — NOT bcrypt; encrypted with the token itself as key)
- `name` (device/client label)
- `token` (hashed session token — see hashing note below)
- `type` (`0` = temporary session, `1` = permanent app token, `2` = wipe token)
- `last_activity` (unix timestamp, updated per request)
- `last_check` (timestamp for periodic password re-validation)
- `scope` (JSON; lockdown scopes such as filesystem-only)
- `expires` (optional expiry timestamp)
- `private_key` / `public_key` (end-to-end encryption keys)

**Token hash algorithm** (source: `lib/private/Authentication/Token/PublicKeyTokenProvider.php:412-421`):
- Primary: `hash('sha512', $token . $secret)` — SHA-512 of the **concatenation** of raw token value + server secret from `config.php`'s `secret` key. This is NOT plain SHA-512 and NOT HMAC.
- Fallback (pre-NC 20 installs without a secret): `hash('sha512', $token)` — plain SHA-512 without the secret suffix.

Token lookup on each request: compute `SHA-512(raw_token || server_secret)`, query `oc_authtoken.token`.

### 4.3 DAV authentication precedence

**Pre-auth phase** (runs before `Auth.php`): `OC::handleLogin()` in `lib/base.php:1225-1255` establishes the logged-in state via these checks in order:
1. Apache auth (`OC_User::handleApacheAuth()`)
2. Token login (`$userSession->tryTokenLogin($request)` — `Session.php:818-862`):
   - If `Authorization: Bearer {token}` → use bearer value as the token
   - Else if the `{instanceid}` cookie is present → use `$this->session->getId()` (the PHP session ID) as the token
   - Hash the token via `hash('sha512', $token . $secret)`, look up in `oc_authtoken`
   - If found: `loginWithToken()` → `setUser()` → sets `$_SESSION['user_id']`
   - Browser sessions create a `type=0` (TEMPORARY_TOKEN) row in `oc_authtoken` via `createSessionToken()` at login time, so the PHP session ID is a valid token
3. Remember-me cookies: requires ALL THREE `$_COOKIE['nc_username']`, `$_COOKIE['nc_token']`, `$_COOKIE['nc_session_id']` → `loginWithCookie($uid, $token, $oldSessionId)` (`Session.php:871-935`)
4. Basic auth (`$userSession->tryBasicAuthLogin($request, $throttler)`)

**DAV auth phase** (`Auth.php::auth()` — `apps/dav/lib/Connector/Sabre/Auth.php:163-197`): runs after `handleLogin()` has (possibly) established a session.
1. CSRF check first (`requiresCSRFCheck()` — see §4.4). POST CSRF failure → forced logout + re-challenge.
2. 2FA check: if `twoFactorManager->needsSecondFactor()` → `401 "2FA challenge not passed."`
3. Session shortcuts (checked in this order):
   - Logged in AND `AUTHENTICATED_TO_DAV_BACKEND` is `null` → accept ("Fix for broken webdav clients" — first DAV request in session, `Auth.php:186`)
   - Logged in AND `AUTHENTICATED_TO_DAV_BACKEND === current UID` AND no `Authorization` header → accept ("Well behaved clients that only send the cookie", `Auth.php:188`)
   - Apache auth → accept
4. Fall through to `parent::check()` (SabreDAV `AbstractBasic::check()` — parses `Authorization: Basic` header, calls `validateUserPass()` which calls `logClientIn()` and sets `AUTHENTICATED_TO_DAV_BACKEND` on success, `Auth.php:91`)
5. Bearer token failure: return HTTP 401 **with no `WWW-Authenticate` header** (unlike Basic auth). Exception: if `oauth2.enable_oc_clients = true` in config and the `User-Agent` contains `mirall`, send a standard `WWW-Authenticate` challenge.

**`AUTHENTICATED_TO_DAV_BACKEND`** stores a **UID string** (not a boolean). Set at `Auth.php:91`: `$this->session->set(self::DAV_AUTHENTICATED, $this->userSession->getUser()->getUID())`. Checked at `Auth.php:63-65`: `$this->session->get(self::DAV_AUTHENTICATED) === $username`. This prevents session fixation when a WebDAV client resends cookies after an account change.

**Response headers on auth failure:**
4. If the request is from an XMLHttpRequest (`X-Requested-With: XMLHttpRequest`) and Basic auth fails → respond with `WWW-Authenticate: DummyBasic realm="…"` (prevents browser pop-up).
5. If not an XMLHttpRequest and Basic auth fails → respond with `WWW-Authenticate: Basic realm="Nextcloud"`.

### 4.4 CSRF checks (DAV context)

CSRF check is **skipped** when:
- Request method is GET, HEAD, or OPTIONS
- `User-Agent` matches Nextcloud desktop, Android, or iOS client patterns (exact regex from `IRequest.php`):
  - Desktop: `/^Mozilla\/5\.0 \([A-Za-z ]+\) (?:mirall|csyncoC)\/([^ ]*).*$/`
  - Android: `/^Mozilla\/5\.0 \(Android\) (?:ownCloud|Nextcloud)\-android\/([^ ]*).*$/`
  - iOS: `/^Mozilla\/5\.0 \(iOS\) (?:ownCloud|Nextcloud)\-iOS\/([^ ]*).*$/`
- User is not logged in
- Request method is **not** POST, and user is logged in and already DAV-authenticated (`AUTHENTICATED_TO_DAV_BACKEND` set)

CSRF check **always required** for POST requests from browser sessions, **regardless** of DAV authenticated state. On a POST CSRF failure, the session is forcibly logged out and the request is re-challenged (not a plain 401).

### 4.5 2FA enforcement

If the authenticated user has a pending 2FA challenge (`oc_twofactor_providers` — only relevant when 2FA app is installed via PHP-FPM), DAV authentication must return `401 Not Authenticated: 2FA challenge not passed.`

### 4.6 Brute-force throttling

- Table: `oc_bruteforce_attempts` (columns: `action`, `occurred`, `ip`, `subnet`, `metadata`)
- On each failed login: `INSERT` a row for action `login`, IP and /24 subnet.
- On each subsequent login attempt: compute delay = `min(25s, 100ms * 2^attempts)` and sleep.
  - `firstDelay = 0.1` (100 ms), formula: `delay = firstDelay * 2^attempts`.
  - Maximum delay cap: **25 seconds** (`IThrottler::MAX_DELAY = 25`, `MAX_DELAY_MS = 25000`).
- 429 trigger: when `attempts > auth.bruteforce.max-attempts` (default **10**, configurable) **and** those same attempts occurred within the last **30 minutes** → throw `MaxDelayReached` → HTTP 429. A bare attempt count over the threshold in the last 12 hours only throttles (sleeps), not 429.
- Allowlist stored as `oc_appconfig` entries: `appid = 'bruteForce'`, config keys prefixed `whitelist_`, values are IP CIDR range strings. Bypass check done before delay calculation.
- Disable if system config `auth.bruteforce.protection.enabled = false`.

---

## 5. OCS API

### 5.1 Envelope format

Both XML and JSON are supported. Format selection:
1. `?format=xml` or `?format=json` query parameter (takes precedence)
2. `Accept: application/json` header
3. Default: XML

#### XML envelope (`Content-Type: text/xml; charset=UTF-8`)

```xml
<?xml version="1.0"?>
<ocs>
  <meta>
    <status>ok</status>
    <statuscode>100</statuscode>
    <message>OK</message>
    <totalitems></totalitems>
    <itemsperpage></itemsperpage>
  </meta>
  <data>…</data>
</ocs>
```

#### JSON envelope (`Content-Type: application/json; charset=utf-8`)

```json
{"ocs":{"meta":{"status":"ok","statuscode":100,"message":"OK"},"data":{…}}}
```

### 5.2 OCS v1 HTTP status mapping

| Condition | HTTP status |
|---|---|
| Any success | 200 |
| Unauthorised (OCS status 997) | 401 |
| Maintenance mode | 503 (exception override) |

OCS `statuscode` 100 = success; `status` field is `"ok"` when statuscode = 100, otherwise `"failure"`.

`totalitems` and `itemsperpage` are included as empty strings in v1 (not absent — existing clients depend on this).

In OCS **v2**, `totalitems` and `itemsperpage` are **omitted** from the meta block unless the handler explicitly sets them. Do not emit the keys as empty strings in v2.

### 5.3 OCS v2 HTTP status mapping

Maps OCS status codes directly to HTTP status codes:
- 200–299 → as-is
- 997 → 401
- 998 → 404
- 999 or unknown → 500
- Outside 200-600 → 400

### 5.4 Unauthorised OCS response headers

Browser requests (`X-Requested-With: XMLHttpRequest`):
```
WWW-Authenticate: DummyBasic realm="Authorisation Required"
```
Other requests:
```
WWW-Authenticate: Basic realm="Authorisation Required"
```

### 5.5 `OCS-APIREQUEST` header

When `OCS-APIREQUEST: true` is present, CSRF token verification is bypassed. This is used by all desktop/mobile clients.

### 5.6 Core OCS endpoints

All mounted at `/ocs/v1.php/` and `/ocs/v2.php/` with the same path suffix.

#### `GET /cloud/capabilities`

Returns merged capabilities from all registered providers. The Rust server must natively return capabilities for:

**core** (from `OC\OCS\CoreCapabilities`):
```json
{
  "core": {
    "pollinterval": 60,
    "webdav-root": "remote.php/webdav",
    "reference-api": true,
    "reference-regex": "…",
    "mod-rewrite-working": true
  }
}
```

**dav** (from `OCA\DAV\Capabilities`):
```json
{
  "dav": {
    "chunking": "1.0",
    "public_shares_chunking": true,
    "search_supports_creation_time": true,
    "search_supports_upload_time": true,
    "bulkupload": "1.0"
  }
}
```

**files** (from `OCA\Files\Capabilities`):
```json
{
  "files": {
    "bigfilechunking": true,
    "blacklisted_files": [],
    "forbidden_filenames": [],
    "forbidden_filename_basenames": [],
    "forbidden_filename_characters": [],
    "forbidden_filename_extensions": [],
    "chunked_upload": {
      "max_size": 10737418240,
      "max_parallel_count": 5
    },
    "file_conversions": []
  }
}
```

Capabilities registered by PHP apps (e.g. `files_sharing`, `provisioning_api`) must be merged in. Rust collects them from PHP-FPM at startup or on capability-invalidating config changes.

**Authentication state matters:** When the request is authenticated, return the full capability set from `getCapabilities()`. When unauthenticated, return only `IPublicCapability` results via `getCapabilities(true)`. The ETag of the response is `md5(json_encode($result))`.

#### `GET /ocs/v1.php/config`

```xml
<ocs>
  <meta>…</meta>
  <data>
    <version>1.7</version>
    <website>Nextcloud</website>
    <host>example.com</host>
    <contact>admin@example.com</contact>
    <ssl>false</ssl>
  </data>
</ocs>
```

#### `POST /person/check`

Login credential validation endpoint (used by ownCloud-compatible federation). Validates `login` + `password` against user database.

#### `GET /identityproof/key/{cloudId}`

Returns the server's public signing key for the given user's cloud ID.

---

## 6. WebDAV / DAV

### 6.1 URL structure

All sub-trees are served by Nextcloud's SabreDAV server (`apps/dav`), which registers a `RootCollection` with named child trees. The Rust native handler serves only the **files** sub-tree; all other sub-trees are forwarded to PHP-FPM.

| URL | Handler | Purpose |
|---|---|---|
| `/remote.php/webdav/…` | **Rust native** | Authenticated WebDAV root (v1) — alias for `/dav/files/{userId}/` |
| `/remote.php/dav/files/{userId}/…` | **Rust native** | User file tree (DAV v2) |
| `/remote.php/dav/versions/{userId}/…` | PHP-FPM | File version history (`files_versions` app) |
| `/remote.php/dav/trashbin/{userId}/…` | PHP-FPM | Trash bin (`files_trashbin` app) |
| `/remote.php/dav/uploads/{userId}/…` | PHP-FPM | Chunked upload v2 staging area |
| `/remote.php/dav/comments/…` | PHP-FPM | File comments |
| `/remote.php/dav/calendars/…` | PHP-FPM | CalDAV |
| `/remote.php/dav/public-calendars/…` | PHP-FPM | Public CalDAV |
| `/remote.php/dav/system-calendars/…` | PHP-FPM | System CalDAV |
| `/remote.php/dav/addressbooks/…` | PHP-FPM | CardDAV |
| `/remote.php/dav/avatars/…` | PHP-FPM | User avatars |
| `/remote.php/dav/principals/…` | PHP-FPM | Principals tree (ACL/CalDAV/CardDAV) |
| `/dav/files/{userId}/…` | **Rust native** | User file tree |
| `/dav/versions/{userId}/…` | PHP-FPM | File version history |
| `/dav/trashbin/{userId}/…` | PHP-FPM | Trash bin |
| `/dav/uploads/{userId}/…` | PHP-FPM | Chunked upload v2 staging area |
| `/dav/comments/…` | PHP-FPM | File comments |
| `/dav/calendars/…` | PHP-FPM | CalDAV |
| `/dav/public-calendars/…` | PHP-FPM | Public CalDAV |
| `/dav/system-calendars/…` | PHP-FPM | System CalDAV |
| `/dav/addressbooks/…` | PHP-FPM | CardDAV |
| `/dav/avatars/…` | PHP-FPM | User avatars |
| `/dav/principals/…` | PHP-FPM | Principals tree |
| `/public.php/webdav/…` | PHP-FPM | Public share WebDAV |
| `/public.php/dav/…` | PHP-FPM | Public share DAV v2 |

### 6.2 RFC 4918 methods required

`OPTIONS`, `PROPFIND`, `PROPPATCH`, `GET`, `HEAD`, `PUT`, `DELETE`, `MKCOL`, `COPY`, `MOVE`, `LOCK`, `UNLOCK`

Additional Nextcloud methods:
- `PATCH` (checksum recalculation via `X-Recalculate-Hash` header)
- `POST` on `/dav/bulk` (bulk upload)
- `SEARCH` (file search via `SearchDAV` library — returns `207 Multi-Status`)
- `REPORT` (delta sync via RFC 6578 `sync-collection` — requires `{DAV:}sync-token` property on collections)

### 6.3 `dav-server-rs` integration

Use the `dav-server` crate (trait-based WebDAV implementation). Implement:

#### `DavFileSystem` trait

Methods: `read_dir`, `metadata`, `get_file`, `put_file`, `remove_file`, `remove_dir`, `create_dir`.

Storage backend: Nextcloud `oc_filecache` table + local/object storage.

Path resolution rules:
- `/dav/files/{userId}/{path}` → resolve `{path}` relative to the user's home storage root (storage ID `home::{userId}`, root path `files/`)
- Node IDs from `oc_filecache.fileid`
- Permissions from `oc_share` + `oc_filecache.permissions` (bitfield: READ=1, UPDATE=2, CREATE=4, DELETE=8, SHARE=16, ALL=31)

#### `DavProp` (via `DavFileSystem`)

See §6.5 for the full required property list.

#### `DavLockSystem` trait

Implement as a no-op (fake locking) — mirrors `FakeLockerPlugin` in SabreDAV:
- `LOCK` returns HTTP 200 with a `lockdiscovery` body containing a fake token derived from `md5(path)`, timeout 1800s.
- `UNLOCK` returns HTTP 204.
- `PROPFIND` for `{DAV:}lockdiscovery` returns an empty lock list.
- `PROPFIND` for `{DAV:}supportedlock` returns locks supported (shared + exclusive, depth infinity).
- Lock tokens in `If:` headers are always validated as valid (no actual locking state).

### 6.4 Response headers after write operations

After any successful `PUT`, `COPY`, `MOVE`, or chunked upload assembly:

| Header | Value |
|---|---|
| `OC-FileId` | `oc_filecache.fileid` as string |
| `ETag` | `"` + md5/hash + `"` |
| `OC-ETag` | same value as ETag (without quotes) |
| `X-OC-MTime: accepted` | only when `X-OC-MTime` header was honored |
| `X-OC-CTime: accepted` | only when `X-OC-CTime` header was honored |
| `OC-Checksum` | `ALGORITHM:hash` on GET responses when checksum is stored |
| `X-Accel-Buffering: no` | on all file GET responses (disables nginx buffering) |
| `Content-Disposition` | `attachment; filename*=UTF-8''...` or `attachment; filename="..."` |

### 6.5 DAV properties

#### Standard DAV properties

| Property | Notes |
|---|---|
| `{DAV:}resourcetype` | `collection` for directories, empty for files |
| `{DAV:}getcontentlength` | file size in bytes |
| `{DAV:}getcontenttype` | MIME type |
| `{DAV:}getetag` | quoted ETag string; **writable via PROPPATCH** (not protected) |
| `{DAV:}getlastmodified` | RFC 1123 date |
| `{DAV:}creationdate` | ISO 8601 atom date |
| `{DAV:}displayname` | node name; **read-only via PROPPATCH** (returns 403) |
| `{DAV:}quota-available-bytes` | free bytes for storage quota |
| `{DAV:}quota-used-bytes` | used bytes |
| `{DAV:}supportedlock` | fake lock types |
| `{DAV:}lockdiscovery` | empty (fake locker) |

#### OwnCloud namespace (`http://owncloud.org/ns` → `oc:`)

All the following are **read-only** (protected) unless noted:

| Property | Description |
|---|---|
| `{oc:}id` | Global file ID: `fileid` zero-padded to 8 chars + instance ID |
| `{oc:}fileid` | Raw numeric `oc_filecache.fileid` |
| `{oc:}permissions` | Encoded permissions string: R (read), W (write), CK (create), D (delete), S (shared), M (mounted), etc. Shares strip S and M for public links |
| `{oc:}size` | Recursive size (directories include children) |
| `{oc:}owner-id` | UID of file owner |
| `{oc:}owner-display-name` | Display name of owner (omitted or null for public links unless scope is published) |
| `{oc:}checksums` | `ALGORITHM:hash` list XML element |
| `{oc:}data-fingerprint` | Config value `data-fingerprint` |
| `{oc:}downloadURL` | Direct download URL (storage-specific) |
| `{oc:}share-permissions` (in `open-collaboration-services.org/ns`) | Integer bitmask of share permissions |
| `{oc:}share-permissions` (OCM, in `open-cloud-mesh.org/ns`) | JSON array of `read`, `write`, `share` |
| `{oc:}share-attributes` (in `http://nextcloud.org/ns`) | JSON share attributes |

#### Nextcloud namespace (`http://nextcloud.org/ns` → `nc:`)

| Property | Description |
|---|---|
| `{nc:}has-preview` | JSON `true`/`false` |
| `{nc:}mount-type` | Mount point type string (`local`, `shared`, `external`, etc.) |
| `{nc:}is-mount-root` | `"true"` if node's internal path is empty (shared root) |
| `{nc:}is-federated` | `"true"` if mount is a federated external share |
| `{nc:}metadata_etag` | ETag of associated metadata. **⚠ Known PHP bug:** `METADATA_ETAG_PROPERTYNAME` is defined in `FilesPlugin` but no `$propFind->handle()` call exists, so this property is never returned by PROPFIND in the reference PHP implementation. Implement it correctly in Rust (read from `oc_filecache_extended.metadata_etag`). |
| `{nc:}upload_time` | Unix timestamp of upload |
| `{nc:}creation_time` | Unix timestamp of creation; **writable via PROPPATCH** |
| `{nc:}note` | Share note from associated share |
| `{nc:}hide-download` | `"true"` if share has hide-download set |
| `{nc:}contained-folder-count` | Count of direct child directories |
| `{nc:}contained-file-count` | Count of direct child files |
| `{nc:}metadata-{key}` | Per-file metadata values from `oc_files_metadata`; writable based on `EDIT_REQ_*` permission level |
| `{nc:}hidden` | `"true"` if file is a live photo MOV companion |
| `{nc:}download-url-expiration` | Unix timestamp when the `{oc:}downloadURL` signed URL expires; absent if no direct download URL is configured. Protected (read-only). |
| `{DAV:}creationdate` | Also writable via PROPPATCH (mapped to `creation_time`) |
| `{DAV:}lastmodified` | Writable via PROPPATCH (updates mtime) |

### 6.6 PROPPATCH writable properties

| Property | Action |
|---|---|
| `{DAV:}lastmodified` | Update file mtime |
| `{DAV:}getetag` | Set custom ETag |
| `{DAV:}creationdate` | Set creation time (ISO 8601 parsed) |
| `{nc:}creation_time` | Set creation time (unix int) |
| `{nc:}metadata-{key}` | Update metadata value (permission-checked) |
| `{DAV:}displayname` | Return 403 (blocked) |

### 6.7 Trash bin on DELETE

DELETE on `/remote.php/dav/files/{userId}/{path}` (and equivalent `/dav/files/{userId}/{path}`) must **not** permanently delete the file. Instead, it must move the file to the trash bin, matching PHP's `TrashbinPlugin` which intercepts `unlink()` calls via the `files_trashbin` storage wrapper.

#### 6.7.1 Disk layout

The file is renamed on disk from:

```
{datadirectory}/{userId}/files/{relative_path}
```

to:

```
{datadirectory}/{userId}/files_trashbin/files/{relative_path}.d{timestamp}
```

For directories, the entire subtree is moved under the same `.d{timestamp}`-suffixed path.

`timestamp` is the current Unix timestamp at deletion time.

If a file with the same trash path already exists, append an incrementing suffix: `.d{timestamp}_1`, `.d{timestamp}_2`, etc.

#### 6.7.2 Filecache update

The `oc_filecache` row is **updated** (not deleted):

| Column | New value |
|---|---|
| `path` | `files_trashbin/files/{relative_path}.d{timestamp}` |
| `path_hash` | `MD5(new_path)` |
| `name` | Original basename with `.d{timestamp}` appended |
| `parent` | `fileid` of the `files_trashbin/files` directory (auto-created if missing) |
| `mtime` | `timestamp` (deletion time) |

The `oc_filecache_extended` row is left unchanged (still keyed by `fileid`).

#### 6.7.3 `oc_files_trash` table

One row is inserted per deleted file/directory:

| Column | Value |
|---|---|
| `auto_id` | auto-increment |
| `id` | `fileid` from `oc_filecache` (the deleted node's fileid) |
| `user` | UID of the deleting user |
| `timestamp` | Unix timestamp of deletion |
| `location` | Original `files/{relative_path}` (the path before deletion) |
| `type` | `'file'` or `'folder'` |
| `deleted_by` | UID of the user who performed the deletion (same as `user` for direct deletes; differs for share recipients) |

#### 6.7.4 Deletion from trash (permanent delete)

DELETE on `/remote.php/dav/trashbin/{userId}/{path}` is forwarded to PHP-FPM (existing route). PHP-FPM's `files_trashbin` app handles the permanent deletion from disk and removal from `oc_files_trash` + `oc_filecache`. The Rust handler does not need to implement permanent-delete logic — the trashbin DAV subtree is already routed to PHP-FPM (§6.1).

#### 6.7.5 Versioning interaction

When a file is deleted and moved to trash, any existing versions in `files_versions/` are preserved as-is by the `files_versions` app (PHP-FPM). The Rust handler does not interact with versions during trash moves.

---

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
- `X-OC-MTime: {timestamp}` (also accepted as `X-File-MTime` for legacy clients)
- `Content-Length: {n}`

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

## 8. Files App REST Endpoints

All mounted under `/apps/files/` (via OC routing, `index.php/apps/files/…` or clean URL).

### 8.1 REST (non-OCS) endpoints

| Method | URL | Description |
|---|---|---|
| `GET` | `/apps/files/` | Files app SPA index page (PHP-FPM) |
| `GET` | `/apps/files/f/{fileid}` | Show file by ID (PHP-FPM redirect) |
| `GET` | `/apps/files/api/v1/thumbnail/{x}/{y}/{file+}` | Generate/fetch preview thumbnail |
| `POST` | `/apps/files/api/v1/files/{path+}` | Update file tags |
| `GET` | `/apps/files/api/v1/recent/` | Recent files list |
| `GET` | `/apps/files/api/v1/stats` | Storage stats (used/free/total) |
| `PUT` | `/apps/files/api/v1/views/{view}/{key}` | Set view config value |
| `PUT` | `/apps/files/api/v1/views` | Set multiple view config values |
| `GET` | `/apps/files/api/v1/views` | Get all view configs |
| `PUT` | `/apps/files/api/v1/config/{key}` | Set user config value |
| `GET` | `/apps/files/api/v1/configs` | Get all user config values |
| `POST` | `/apps/files/api/v1/showhidden` | Toggle show-hidden-files |
| `POST` | `/apps/files/api/v1/cropimagepreviews` | Toggle crop image previews |
| `POST` | `/apps/files/api/v1/showgridview` | Set grid view |
| `GET` | `/apps/files/api/v1/showgridview` | Get grid view setting |
| `GET` | `/apps/files/directEditing/{token}` | Direct editing token view (PHP-FPM) |
| `GET` | `/apps/files/preview-service-worker.js` | Service worker JS |
| `GET` | `/apps/files/{view}` | View-specific SPA entry (PHP-FPM) |
| `GET` | `/apps/files/{view}/{fileid}` | View+fileid SPA entry (PHP-FPM) |

### 8.2 OCS endpoints (mounted under `/ocs/…/apps/files/api/v1/`)

| Method | URL suffix | Description |
|---|---|---|
| `GET` | `/directEditing` | Direct editing info (available editors) |
| `GET` | `/directEditing/templates/{editorId}/{creatorId}` | Templates for editor |
| `POST` | `/directEditing/open` | Open file in direct editor |
| `POST` | `/directEditing/create` | Create file via direct editor |
| `GET` | `/templates` | List file templates |
| `GET` | `/templates/fields/{fileId}` | List template fields for file |
| `POST` | `/templates/create` | Create file from template |
| `POST` | `/templates/path` | Set templates folder path |
| `POST` | `/transferownership` | Initiate ownership transfer |
| `POST` | `/transferownership/{id}` | Accept transfer |
| `DELETE` | `/transferownership/{id}` | Reject transfer |
| `POST` | `/openlocaleditor` | Create open-in-local-editor token |
| `POST` | `/openlocaleditor/{token}` | Validate open-in-local-editor token |

---

## 9. Database Schema

The Rust server manages the following tables (minimum required for core + files). All table names use the `oc_` prefix by default (configurable via `dbtableprefix`).

### 9.1 Users and accounts

**`oc_users`**
- `uid` VARCHAR(64) PK
- `displayname` VARCHAR(64)
- `password` VARCHAR(255) — hashed (bcrypt)
- `uid_lower` VARCHAR(64)

**`oc_accounts`**
- `uid` VARCHAR(64) PK
- `data` LONGTEXT (JSON blob of account properties)

**`oc_accounts_data`**
- `id` BIGINT PK AI
- `uid` VARCHAR(64)
- `name` VARCHAR(64)
- `value` MEDIUMTEXT
- `verified` SMALLINT

**`oc_groups`**
- `gid` VARCHAR(255) PK

**`oc_group_user`**
- `gid` VARCHAR(255)
- `uid` VARCHAR(64)

### 9.2 Auth tokens

**`oc_authtoken`**
- `id` BIGINT PK AI
- `uid` VARCHAR(64) NOT NULL
- `login_name` VARCHAR(255) NOT NULL
- `password` VARCHAR(1024) — encrypted password (for token refresh)
- `name` VARCHAR(128) NOT NULL — device label
- `token` VARCHAR(200) NOT NULL UNIQUE — SHA-512 of token value
- `type` SMALLINT — 0=temporary, 1=permanent, 2=wipe
- `remember` SMALLINT — whether to persist session
- `last_activity` INT — unix ts
- `last_check` INT — unix ts
- `scope` VARCHAR(128) — JSON lockdown scope
- `expires` INT — optional expiry
- `private_key` TEXT
- `public_key` TEXT
- `version` SMALLINT

**`oc_bruteforce_attempts`**
- `id` BIGINT PK AI
- `action` VARCHAR(64)
- `occurred` INT
- `ip` VARCHAR(255)
- `subnet` VARCHAR(255)
- `metadata` VARCHAR(255) — JSON

### 9.3 App config and user preferences

**`oc_appconfig`**
- `appid` VARCHAR(32)
- `configkey` VARCHAR(64)
- `configvalue` CLOB/TEXT
- `type` INT (1=string, 2=int, 4=float, 8=bool, 16=array)
- `lazy` SMALLINT

**`oc_preferences`**
- `userid` VARCHAR(64)
- `appid` VARCHAR(32)
- `configkey` VARCHAR(64)
- `configvalue` CLOB/TEXT
- `type` INT
- `lazy` SMALLINT
- `flags` INT

### 9.4 File storage and cache

**`oc_storages`**
- `numeric_id` BIGINT PK AI
- `id` VARCHAR(64) UNIQUE — e.g. `home::alice`, `object::store::s3::…`
- `available` SMALLINT
- `last_checked` INT

**`oc_filecache`**
- `fileid` BIGINT PK AI
- `storage` BIGINT FK `oc_storages.numeric_id`
- `path` VARCHAR(4000)
- `path_hash` VARCHAR(32) — md5 of path; unique with storage
- `parent` BIGINT FK self
- `name` VARCHAR(250)
- `mimetype` BIGINT FK `oc_mimetypes.id`
- `mimepart` BIGINT FK `oc_mimetypes.id`
- `size` BIGINT — -1 = unscanned
- `mtime` INT
- `storage_mtime` INT
- `encrypted` SMALLINT
- `unencrypted_size` BIGINT
- `etag` VARCHAR(40)
- `permissions` INT — CRUDS bitmask
- `checksum` VARCHAR(255)

> **Note:** `creation_time` and `upload_time` are **not** columns of `oc_filecache`. They live exclusively in `oc_filecache_extended` (added in NC 17 via `Version17000Date20190514105811`). Do not SELECT them from `oc_filecache`.

**`oc_mimetypes`**
- `id` BIGINT PK AI
- `mimetype` VARCHAR(255) UNIQUE

**`oc_filecache_extended`**
- `fileid` BIGINT PK FK `oc_filecache.fileid`
- `metadata_etag` VARCHAR(40)
- `creation_time` INT — authoritative source for `{nc:}creation_time`; this is the **only** table that has this column
- `upload_time` INT — authoritative source for `{nc:}upload_time`; this is the **only** table that has this column

**`oc_files_trash`**
- `auto_id` BIGINT PK AI
- `id` BIGINT NOT NULL — `oc_filecache.fileid` of the trashed node
- `user` VARCHAR(64) NOT NULL — UID of the user who deleted the file
- `timestamp` INT NOT NULL — Unix timestamp of deletion
- `location` VARCHAR(512) NOT NULL — original path before deletion (e.g. `files/Documents/report.pdf`)
- `type` VARCHAR(8) — `'file'` or `'folder'`
- `deleted_by` VARCHAR(64) — UID of the user who performed the deletion (same as `user` for direct deletes)

**`oc_files_metadata`**
- `id` BIGINT PK AI
- `file_id` BIGINT
- `json` LONGTEXT
- `sync_token` VARCHAR(15)
- `last_update` DATETIME

### 9.5 DAV properties

**`oc_properties`**
- `id` BIGINT PK AI
- `userid` VARCHAR(64)
- `propertypath` VARCHAR(255)
- `propertyname` VARCHAR(255)
- `propertyvalue` MEDIUMTEXT
- `valuetype` SMALLINT

### 9.6 Shares

**`oc_share`**
- `id` BIGINT PK AI
- `share_type` SMALLINT
- `share_with` VARCHAR(255)
- `uid_owner` VARCHAR(64)
- `uid_initiator` VARCHAR(64)
- `parent` BIGINT
- `item_type` VARCHAR(64)
- `item_source` VARCHAR(255)
- `item_target` VARCHAR(255)
- `file_source` BIGINT
- `file_target` VARCHAR(512)
- `permissions` INT
- `stime` BIGINT
- `accepted` SMALLINT
- `expiration` DATETIME
- `token` VARCHAR(32)
- `mail_send` SMALLINT
- `note` MEDIUMTEXT
- `label` VARCHAR(255)
- `attributes` MEDIUMTEXT
- `hide_download` SMALLINT
- `password` VARCHAR(255)
- `password_by_talk` SMALLINT

**`oc_share_external`** (federated shares — queried by PHP-FPM app)

### 9.8 Two-factor auth (required for DAV auth enforcement)

**`oc_twofactor_providers`**
- `provider_id` VARCHAR(64)
- `uid` VARCHAR(64)
- `enabled` SMALLINT

This table is read during DAV authentication (§4.5) to check if the user has a pending 2FA challenge. The Rust server must query it even though the 2FA apps themselves are managed by PHP-FPM.

### 9.7 Migration strategy

- Use `sqlx::migrate!()` with versioned SQL files (one file per schema change, named with timestamp).
- Migrations are idempotent: check `sqlx_migrations` table (created automatically by sqlx) for applied versions.
- Interop with PHP Nextcloud databases: SQL migration files must be additive only when the schema already exists. Never drop or rename existing columns.
- On fresh install: create all tables from scratch, then:
  - Write to **`config.php`** (via `SystemConfig::setValue`): `installed = true`, `instanceid`, `secret`, `passwordsalt`
  - Write to **`oc_appconfig`**: `core / oc_version = {version}`, `core / versionstring = {versionstring}`, `core / lastupdatedat = {timestamp}`, `core / installedat = {microtime}`
  - An admin user record in `oc_users` and `oc_accounts`
- Supported DBs: PostgreSQL, MySQL/MariaDB, SQLite.

---

## 10. PHP-FPM Integration

### 10.1 Architecture

Rust is the sole listener on port 80/443. For routes not handled natively:
1. Rust looks up the URL prefix in its route registry.
2. If the prefix maps to a PHP app, Rust forwards the request to PHP-FPM over FastCGI (Unix socket or TCP).
3. PHP-FPM runs a lightweight Nextcloud bootstrap shim (not the full `OC::handleRequest()` stack).
4. Rust returns PHP-FPM's response verbatim to the client.

### 10.2 Auth handoff to PHP

Auth is performed entirely by Rust. After a request is authenticated:
- FastCGI param `HTTP_X_NC_USER = {userId}`
- FastCGI param `HTTP_X_NC_SESSION_TOKEN = {tokenValue}`
- FastCGI param `HTTP_X_NC_IS_ADMIN = 0|1`

The PHP-FPM bootstrap shim trusts these params and sets up the `IUserSession` accordingly without re-authenticating.

### 10.3 PHP bootstrap shim requirements

The shim must provide working implementations (backed by the same DB) for at minimum:
- `IRequest` — wraps FastCGI request
- `IConfig` — reads from `oc_appconfig` / `oc_preferences`
- `IDBConnection` — connects to same DB as Rust
- `IUserSession` — returns the injected `userId` as the current user
- `IUserManager` — looks up users from `oc_users`
- `IGroupManager` — looks up groups from `oc_groups`
- `IAppManager` — loads apps from disk
- `IURLGenerator` — generates URLs relative to the known web root

### 10.4 Route registry

At startup Rust scans `apps/*/appinfo/routes.php` (or a pre-built JSON manifest) and builds a route table:
- paths matching native Rust handlers → handled natively
- all other app routes → forwarded to PHP-FPM
- `SCRIPT_FILENAME` FastCGI param points to a single PHP bootstrap entrypoint, not the app PHP file directly

### 10.5 PHP apps that remain in PHP-FPM

| App | Routes |
|---|---|
| `files_sharing` | `/apps/files_sharing/…`, `/ocs/…/apps/files_sharing/…` |
| `provisioning_api` | `/ocs/…/cloud/users`, `/ocs/…/cloud/groups` |
| `comments` | `/apps/comments/…` |
| `systemtags` | `/apps/systemtags/…` |
| `federation` | `/apps/federation/…` |
| `federatedfilesharing` | `/apps/federatedfilesharing/…` |
| `dav` (CalDAV/CardDAV/comments/avatars) | `/remote.php/dav/calendars/…`, `/remote.php/dav/public-calendars/…`, `/remote.php/dav/system-calendars/…`, `/remote.php/dav/addressbooks/…`, `/remote.php/dav/comments/…`, `/remote.php/dav/avatars/…`, `/remote.php/dav/principals/…`, `/remote.php/dav/uploads/…` and `/dav/` equivalents |
| `settings` | `/settings/…` |
| `files_versions` | `/apps/files_versions/…`, `/ocs/…/apps/files_versions/…`, `/remote.php/dav/versions/…`, `/dav/versions/…` |
| `files_trashbin` | `/apps/files_trashbin/…`, `/ocs/…/apps/files_trashbin/…`, `/remote.php/dav/trashbin/…`, `/dav/trashbin/…` |
| Any other installed app | All its registered routes |

---

## 11. Quota Enforcement

Quota checks happen before write operations (PUT, MKCOL, COPY, MOVE, chunk assembly):

1. Resolve the free space for the target path by querying the storage's `free_space()`.
2. Compare against the larger of `Content-Length`, `X-Expected-Entity-Length`, and `OC-Total-Length` headers.
3. If free space < required space → `507 Insufficient Storage` response.
4. For MKCOL: check 4096 bytes as a proxy for directory creation cost.
5. Storage abstraction sentinel values — when `free_space()` returns **any negative value**, the quota check is skipped and the write is allowed. Sentinels are:
   - `SPACE_NOT_COMPUTED = -1`: file size not yet scanned
   - `SPACE_UNKNOWN = -2`: free space is not determinable (e.g. external storage)
   - `SPACE_UNLIMITED = -3`: no quota / unlimited storage
   - PHP `QuotaPlugin.checkQuota()` treats all negative `free_space()` as "allow" — this mirrors that behavior.
6. DAV property mapping: `{DAV:}quota-available-bytes` reports `-3` when quota is unlimited (confirmed by integration tests). Internal `SPACE_UNKNOWN (-2)` maps to `-3` in the DAV response.

---

## 12. Filename Validation

Before any write (PUT, MKCOL, MOVE, COPY target), validate against:
- `forbidden_filenames` list (exact matches, case-insensitive)
- `forbidden_filename_basenames` (name without extension, case-insensitive)
- `forbidden_filename_characters` (reject names containing any of these characters)
- `forbidden_filename_extensions` (file extension matches, case-insensitive)

Lists are configurable via `oc_appconfig` for `core` app, with defaults matching `.htaccess`, `web.config`, etc.

On violation: `422 Unprocessable Entity` (SabreDAV `InvalidPath` exception → HTTP 400/422).

---

## 13. Checksum Support

### 13.1 On PUT upload

Client may send `OC-Checksum: {ALGORITHM}:{hash}` (MD5, SHA1, SHA256, Adler32 supported).

Server must:
1. Compute the same hash of the received data.
2. If mismatch: return `400 Bad Request`.
3. If match: store the checksum in `oc_filecache.checksum`.

### 13.2 On GET download

Server must include `OC-Checksum: {stored_checksum}` in the response headers if a checksum is stored.

### 13.3 Checksum recalculation (PATCH)

```
PATCH /dav/files/{userId}/{path}
X-Recalculate-Hash: {algorithm}
```

Server recomputes the hash, stores it, and responds:
```
HTTP 204 No Content
OC-Checksum: {ALGORITHM}:{new_hash}
```

---

## 14. Special DAV Plugins

### 14.1 AppleQuirksPlugin

Detect macOS DAV client user-agents (prefix `macOS/`) and fix a specific quirk: when a macOS Calendar or Contacts app sends a `{DAV:}principal-property-search` REPORT to a random principal collection **without** the `applyToPrincipalCollectionSet` flag, force-set the flag to `true`. This is not about stripping headers.

### 14.2 BlockLegacyClientPlugin

Block clients outside a configured version range. Return `403 Forbidden` with an HTML body containing a link to download the supported client version.
- Below `minimum.supported.desktop.version` config (default `3.1.81`): blocked.
- Above `maximum.supported.desktop.version` config (default `99.99.99`): blocked.

### 14.3 RequestIdHeaderPlugin / UserIdHeaderPlugin

Add `X-Request-Id` and `X-Nextcloud-User-Id` headers to all responses for tracing.

### 14.4 CopyEtagHeaderPlugin

Mirror `ETag` value also as `OC-ETag` on every response that includes an `ETag`.

### 14.5 AnonymousOptionsPlugin

Handles unauthenticated `OPTIONS` and `HEAD` requests from **Microsoft Office** user-agents (identified by `Microsoft Office` in the User-Agent string, with empty or bare-Bearer `Authorization`). Sets up a fake empty tree and returns a valid OPTIONS response so Office can probe the DAV endpoint without triggering an authentication popup. This is not a general CORS preflight handler.

### 14.6 DummyGetResponsePlugin

Intercepts any `GET` request on the DAV tree (registered at priority 200). Returns HTTP 200 with plain-text body:
```
This is the WebDAV interface. It can only be accessed by WebDAV clients such as the Nextcloud desktop sync client.
```
No debug-mode condition. This prevents SabreDAV's built-in HTML directory browser from being shown to web browsers.

### 14.8 FilesDropPlugin

Enforce upload-only restrictions on file-drop public shares served via `/public.php/dav`. Logic:
- Allowed methods: `PUT`, `MKCOL`, and `MOVE` (MOVE only for chunked upload assembly where the path starts with `/uploads/`).
- All other methods throw `MethodNotAllowed` (**HTTP 405**).
- Additional features: nickname header (`X-NC-Nickname`) support, automatic path rewriting to put files under offerer's subfolder, conflict resolution (deduplicating filenames), and transparent folder-creation for nested paths.

### 14.7 PropFindPreloadNotifyPlugin / PropfindCompressionPlugin

Optional optimisation: preload related nodes before PROPFIND depth-1 responses; compress PROPFIND response bodies with gzip if client accepts.

---

## 15. Security Headers

### 15.1 DAV endpoints

```
Content-Security-Policy: default-src 'none';
```

### 15.2 Session cookies and CSRF

Nextcloud uses a double-submit cookie + server-side token pattern. Cookies set on browser sessions:

| Cookie | Set by | Value | Notes |
|---|---|---|---|
| `{instanceid}` (e.g., `oc1a2b3c4d5e`) | PHP `session_start()` via `session_name(OC_Util::getInstanceId())` (`lib/base.php:437,447`) | PHP session ID | **The actual PHP session cookie.** Named after `config.php`'s `instanceid` key. Used by `tryTokenLogin()` to detect an existing session. `cookieCheckRequired()` checks for this cookie (via `session_name()`) to trigger the SameSite guard. |
| `oc_sessionPassphrase` | `CryptoWrapper.php:40,56` | Random 128-char string | Encryption key for `CryptoSessionData`. Used to decrypt `$_SESSION` data. Session-scoped (no `Max-Age`). |
| `nc_session_id` | `setMagicInCookie()` (`Session.php:1012`) | Copy of `session_id()` | **Not the PHP session cookie.** Remember-me cookie — stores the old session ID so `loginWithCookie()` can call `renewSessionToken($oldSessionId, $newSessionId)`. |
| `nc_token` | `setMagicInCookie()` (`Session.php:1002`) | Random 32-char string | Remember-me token. Validated against `oc_preferences` entries with `appid='login_token'`, `configkey=$token`, `configvalue=$timestamp`. Rotated on each successful use. |
| `nc_username` | `setMagicInCookie()` (`Session.php:993`) | UID string | Remember-me username. Used by `loginWithCookie()` to look up the user. |
| `nc_sameSiteCookielax` | `base.php` | `"true"` | Dummy cookie, `SameSite=lax` — note **lowercase** `l` and `s` in the cookie name |
| `nc_sameSiteCookiestrict` | `base.php` | `"true"` | Dummy cookie, `SameSite=strict` — note **lowercase** `l` and `s` in the cookie name |

> **Note on cookie name casing:** The actual cookie names use all-lowercase suffixes (`nc_sameSiteCookielax`, `nc_sameSiteCookiestrict`), not camelCase. On HTTPS with path `/`, the `__Host-` prefix is prepended (`__Host-nc_sameSiteCookielax`).

The `cookieCheckRequired()` logic (`lib/private/AppFramework/Http/Request.php:466-474`): the SameSite cookie check is **only triggered** when a request includes either the `{instanceid}` cookie (accessed via `session_name()`) or `nc_token`. If neither is present, the check is bypassed.

The `passesStrictCookieCheck()` verifies both `nc_sameSiteCookiestrict=true` and `nc_sameSiteCookielax=true` are present.

Bypass rules (no strict cookie check required):
- `OCS-APIREQUEST: true` header present (`cookieCheckRequired()` returns false — `Request.php:467-469`)
- `Authorization: Bearer …` header present (token-authenticated requests — no session cookies sent)
- User-Agent matches WebDAVFS or Microsoft-WebDAV-MiniRedir (`base.php:578-580` — skips the entire SameSite check block)

**SameSite check scope** (`base.php:582-608`): the strict cookie check is enforced based on the processing script. `index.php`, `cron.php`, and `public.php` **skip** the check (return early at `base.php:590-593`). **All other scripts** — including `remote.php`, `ocs/v1.php`, `ocs/v2.php` — **enforce** `passesStrictCookieCheck()` and return **HTTP 412 Precondition Failed** (not 401) on failure.

> ~~The `SameSiteCookieMiddleware` only applies to requests routed through `index.php` — not to DAV or OCS routes directly.~~ **WRONG** — see above. The `base.php` enforcement applies to DAV and OCS routes. The `SameSiteCookieMiddleware` in the AppFramework is a separate layer for `index.php` routes only, but `base.php` has its own enforcement for all other scripts.

CSRF token check (separate from cookie check):
- Failure on non-POST → HTTP 401
- Failure on POST → force re-authentication: `userSession->logout()`, then re-challenge

---

## 16. Caching Strategy

### 16.1 In-process cache (Rust `Arc<RwLock<…>>`)

| Cache | Invalidation trigger |
|---|---|
| Route registry (app route map) | App enable/disable, config reload |
| Capabilities payload | Config change in `oc_appconfig`, app enable/disable |
| Auth token hot cache (token hash → uid) | Token revocation or expiry; TTL ≤ 5 minutes |
| Mime type map (`oc_mimetypes`) | Table change (rare; startup + periodic) |
| App config values (`oc_appconfig`) | Write to the same key |
| User quota values | Write to quota config key |

### 16.2 Distributed cache (PHP compat)

Chunked upload v2 metadata **requires** a distributed cache (Redis or Memcached) configured in Nextcloud config as `memcache.distributed`. Rust must use the same cache backend. If not configured, chunked upload v2 must be disabled (capability `dav.chunking` still reported as `1.0` for v1 compat).

---

## 17. Logging and Observability

- Structured log entries for every request: method, path, status code, authenticated user, response time, request ID.
- Log at `DEBUG` level: cache hits/misses, token validation outcome.
- Log at `INFO` level: successful logins, file mutations (create/delete/move).
- Log at `WARN` level: brute-force attempts detected, quota near-limit writes.
- Log at `ERROR` level: storage errors, DB errors, unexpected panics.
- Request ID: generate a UUID per request, propagate as `X-Request-Id` header and in all log lines for that request.

---

## 18. Configuration File

The Rust server reads `config/config.php` (for compat with existing Nextcloud installations) or an equivalent TOML/YAML file for fresh installs. Required keys:

| Key | Type | Description |
|---|---|---|
| `dbtype` | string | `pgsql`, `mysql`, `sqlite3` |
| `dbhost` | string | DB host (+ optional `:port`) |
| `dbname` | string | DB name |
| `dbuser` | string | DB username |
| `dbpassword` | string | DB password |
| `dbtableprefix` | string | Default `oc_` |
| `datadirectory` | string | Absolute path to Nextcloud data dir |
| `installed` | bool | |
| `maintenance` | bool | |
| `version` | string | Nextcloud version string |
| `trusted_domains` | array | Allowed Host header values |
| `overwrite.cli.url` | string | Public base URL |
| `htaccess.IgnoreFrontController` | bool | Clean URLs active |
| `pollinterval` | int | Long-poll interval (default 60) |
| `auth.bruteforce.protection.enabled` | bool | Default true |
| `auth.bruteforce.allowlist` | array | IPs/subnets exempt from throttle |
| `memcache.distributed` | string | Distributed cache class name |
| `redis` | object | Redis connection config |
| `memcached_servers` | array | Memcached server list |
| `data-fingerprint` | string | Reported in DAV `{oc:}data-fingerprint` |
| `bulkupload.enabled` | bool | Default true |
| `oauth2.enable_oc_clients` | bool | Default false |
| `loglevel` | int | 0=DEBUG … 4=FATAL |
| `logfile` | string | Log file path |
| `instanceid` | string | Instance identifier (e.g., `oc1a2b3c4d5e`). Used as PHP `session_name()` — the session cookie is named after this value. Also used in `{oc:}id` DAV property (zero-padded fileid + instanceid). Auto-generated on first install as `'oc' + random(10)`. Required for session cookie detection. |
| `secret` | string | Server secret. Used in token hash: `hash('sha512', $token . $secret)` (`PublicKeyTokenProvider.php:414`). Required for all auth token lookups against `oc_authtoken`. Auto-generated on install. |

---

## 19. Compatibility Test Matrix

The following existing test suites serve as the acceptance criteria (no new test infrastructure needed for protocol compliance):

### Integration (Behat/Gherkin)

| Suite | Covers |
|---|---|
| `build/integration/features/maintenance-mode.feature` | Maintenance mode HTTP behavior |
| `build/integration/features/ocs-v1.feature` | OCS v1 envelope and endpoints |
| `build/integration/features/auth.feature` | All auth flows |
| `build/integration/capabilities_features/capabilities.feature` | Capabilities endpoint |
| `build/integration/dav_features/webdav-related.feature` | WebDAV v1 |
| `build/integration/dav_features/dav-v2.feature` | WebDAV v2, chunking v2 |
| `build/integration/dav_features/dav-v2-public.feature` | Public share DAV |
| `build/integration/dav_features/principal-property-search.feature` | Principal lookup |
| `build/integration/files_features/checksums.feature` | Checksum upload/download |
| `build/integration/files_features/download.feature` | File download |
| `build/integration/files_features/metadata.feature` | File metadata |
| `build/integration/files_features/tags.feature` | Tagging via DAV |
| `build/integration/files_features/transfer-ownership.feature` | Ownership transfer |
| `build/integration/ratelimiting_features/ratelimiting.feature` | Rate limiting / brute-force |
| `build/integration/routing_features/apps-and-routes.feature` | Route resolution |
| `build/integration/sharing_features/*.feature` | Sharing (PHP-FPM proxied) |
| `build/integration/features/provisioning-v1.feature` | Provisioning API (PHP-FPM proxied) |
| `build/integration/features/provisioning-v2.feature` | Provisioning API v2 (PHP-FPM proxied) |

### UI / End-to-End (Cypress)

| Suite | Covers |
|---|---|
| `cypress/e2e/files/*.cy.ts` | Files app UI: navigation, upload, download, rename, delete, search, sort, settings |
| `cypress/e2e/core/*.cy.ts` | Core platform behavior |

### Unit tests (PHP, as behavior oracles)

| Path | Use |
|---|---|
| `apps/dav/tests/unit/**` | Reference for exact DAV property values and edge cases |
| `tests/Core/**` | Reference for auth, OCS, and config behavior |

---

## 20. Non-Functional Requirements

| Requirement | Target |
|---|---|
| Cold start time | < 500 ms on standard server hardware |
| Request latency (PROPFIND depth-1, 100 files) | < 50 ms p99 with warm DB connection pool |
| Concurrent connections | ≥ 10,000 (Tokio async; no thread-per-request) |
| DB connection pool | Min 5, max 50 per process |
| Memory foot print (idle) | < 64 MB resident |
| Binary size | < 50 MB stripped ELF |
| Compile-time DB driver selection | Feature flags: `postgres`, `mysql`, `sqlite` |
| Zero unsafe code outside FFI boundary | `#![forbid(unsafe_code)]` except designated FFI modules |
| Graceful shutdown | Drain in-flight requests within 30 s on SIGTERM |
| Upgrade safety | Rolling deploy: new binary must accept sessions issued by old binary |
