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

> **Cross-cutting caveat (see [`06-webdav-dav.md`](06-webdav-dav.md) §6.7\u2013§6.10 and the "Requirement Gap" note in [`requirements/README.md`](README.md)):** delegating an app's *routes* to PHP-FPM does **not** delegate the parts of that app that execute inline on the Rust-native files subtree. Specifically `files_trashbin` (move-to-trash on `DELETE`, §6.7), `files_versions` (copy-on-overwrite on `PUT`/`MOVE`/`COPY`, §6.9), and the PROPFIND enrichment / `filter-files` REPORT served for `comments`, `systemtags` and file favorites (§6.5.1, §6.10) have a Rust-native write-/read-side even though their own routes below stay in PHP-FPM.

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

---

Prev: [`09-database-schema.md`](09-database-schema.md) · Up: [`README.md`](README.md) · Next: [`11-quota-enforcement.md`](11-quota-enforcement.md)
