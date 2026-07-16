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

> **⚠ Cross-cutting exception:** the app boundary above is drawn by *route ownership*, but a few delegated apps have behaviour that executes **inline on the Rust-native files subtree** and therefore has a Rust-native slice even though the app's own routes stay in PHP-FPM. These are: `files_trashbin` (move-to-trash on `DELETE`, §6.7), `files_versions` (copy-on-overwrite on `PUT`/`MOVE`/`COPY`, §6.9), core filecache **ETag/size/mtime propagation** (§6.8), and the web-client PROPFIND/REPORT enrichment for favorites, `comments`, `systemtags` and shares (§6.5.1, §6.10). See the "Requirement Gap" note in [`requirements/README.md`](README.md).

---

---

Up: [`README.md`](README.md) · Next: [`02-http-entry-points.md`](02-http-entry-points.md)
