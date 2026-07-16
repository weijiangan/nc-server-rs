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

---

Up: [`README.md`](README.md) · Next: [`02-http-entry-points.md`](02-http-entry-points.md)
