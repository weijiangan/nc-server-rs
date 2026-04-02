# Rust Nextcloud Core+Files Implementation Plan

## Repository layout

The Rust implementation lives in `core-rs/` at the root of the `nc-server` repo, alongside the existing PHP codebase. The PHP files are unchanged. The two coexist in the same repository and share nothing at the source level — the integration point is the running system (FastCGI socket, shared DB, shared `datadirectory`).

```
nc-server/
├── apps/                   # PHP apps (unchanged)
├── lib/                    # PHP core library (unchanged)
├── config/                 # config.php (read by both PHP and Rust)
├── core-rs/                # Rust implementation
│   ├── Cargo.toml          # workspace root
│   ├── crates/
│   │   ├── nc-server/      # binary crate — HTTP server, main entry point
│   │   ├── nc-db/          # DB pool, migrations, startup caches
│   │   ├── nc-auth/        # auth stack, token cache, brute-force
│   │   ├── nc-dav/         # DavFileSystem, DavProp, DavLockSystem impls
│   │   ├── nc-ocs/         # OCS envelope, capabilities, core endpoints
│   │   ├── nc-files/       # files app REST + OCS endpoints, upload flows
│   │   └── nc-fastcgi/     # FastCGI client, route registry, PHP-FPM dispatch
│   ├── migrations/         # sqlx versioned SQL migration files
│   └── tests/              # integration test harness (wraps Behat/Cypress)
├── build/integration/      # existing Behat suites (reused as-is)
├── cypress/                # existing Cypress suites (reused as-is)
└── SPECS/                  # this document and related specs
```

### Coexistence rules
- `core-rs/` is a standalone Cargo workspace. Running `cargo build` inside it has no effect on the PHP codebase.
- The Rust binary reads `../config/config.php` relative to its working directory when run from the repo root, or an explicit `--config` path flag.
- `.gitignore` at the repo root gains `core-rs/target/` to exclude Rust build artefacts.
- No PHP files are modified. The PHP codebase continues to work independently (for reference, testing, and PHP-FPM dispatch).

---

## Key dependencies

| Crate | Role |
| --- | --- |
| `axum` | HTTP server framework — chosen over `actix-web` because it is built directly on `tokio`/`hyper` with no separate runtime, giving clean integration with `sqlx` and `dav-server` |
| [`dav-server`](https://github.com/messense/dav-server-rs) | WebDAV handler (RFC4918 litmus-passing); plug in via `DavFileSystem` + `DavProp` + `DavLockSystem` traits |
| `tokio` | Async runtime — the core architectural win: tasks yield at every `.await`, so two OS threads service thousands of concurrent sync clients without worker exhaustion |
| `hyper` | Low-level HTTP types (compatible with `dav-server`) |
| `sqlx` | Async DB access (Nextcloud DB schema); persistent connection pool shared across all Tokio tasks |
| `serde` / `quick-xml` | OCS JSON/XML serialization |

`dav-server-rs` eliminates reimplementing WebDAV from scratch. We implement Nextcloud's storage and property model by satisfying its trait interfaces, and the library handles all RFC4918 methods, preconditions, partial transfers, locking, and the litmus test surface.

---

## 0) DB schema ownership and migrations

The Rust server must be able to both connect to an existing Nextcloud DB and create one from scratch for fresh installs. No PHP is involved in either case.

### Coding steps
1. Extract the minimal Nextcloud DB schema needed for core+files into versioned SQL migration files:
   - `oc_storages`, `oc_filecache`, `oc_filecache_extended` (authoritative `creation_time`/`upload_time`)
   - `oc_mimetypes` (required by `oc_filecache` foreign keys)
   - `oc_files_metadata` (per-file metadata, `{nc:}metadata-{key}` properties)
   - `oc_accounts`, `oc_users`, `oc_groups`, `oc_group_user`
   - `oc_authtoken`, `oc_bruteforce_attempts`
   - `oc_twofactor_providers` (read during DAV auth for 2FA enforcement)
   - `oc_share`, `oc_share_external`
   - `oc_properties` (DAV custom properties)
   - `oc_appconfig`, `oc_preferences`
2. Use `sqlx::migrate!()` to apply migrations at startup — creates the schema if absent, no-ops if already up to date, applies deltas otherwise.
3. Support PostgreSQL and SQLite; driver selected via connection string in config. MySQL/MariaDB is out of scope for this implementation.
4. For existing Nextcloud installs: connect, run `migrate!()` — it detects the current schema version and skips already-applied migrations.
5. For fresh installs: `migrate!()` creates all tables from scratch, then the Rust setup flow writes initial `oc_appconfig` values (version, installed flag, etc.).
6. At startup, populate the process-lifetime in-memory caches that are owned by the DB layer:
   - Mime-type map: `SELECT id, mimetype FROM oc_mimetypes` → `Arc<RwLock<HashMap<String, i64>>>`. Invalidated only on write (mime types change rarely). Eliminates a per-request join on every PROPFIND row.
   - App config hot values: `SELECT appid, configkey, configvalue FROM oc_appconfig WHERE lazy = 0` → `Arc<RwLock<HashMap>>`. Invalidated on any config write. Covers `maintenance`, `data-fingerprint`, forbidden filename lists, quota defaults.

### Verification steps
- Unit-test each migration file: apply to empty DB, verify expected tables/columns exist.
- Smoke-test against a PHP-created Nextcloud DB: run migrations, confirm no destructive changes, run the Behat/Gherkin suite normally.
- Smoke-test a fresh DB: run migrations from scratch, confirm the server starts and passes `build/integration/features/maintenance-mode.feature` and `/status.php` checks.

---

## 1) Stand up the HTTP server skeleton

### Coding steps
1. Create a Rust HTTP server exposing the same entry points:
	- `/index.php`
	- `/remote.php/{service}/...`
	- `/public.php/{service}/...`
	- `/ocs/v1.php/...`
	- `/ocs/v2.php/...`
	- `/status.php`
	- `/heartbeat`
	- `GET /ocs-provider/index.php` — JSON list of available OCS providers
	- `GET /.well-known/{service}` — webfinger, nodeinfo; DAV PROPFIND redirects for caldav/carddav (all PHP-FPM, but Rust must route them)
	- Login flow routes: `GET|POST /login/flow`, `GET /login/flow/grant`, `POST /login/v2/poll`, `GET /login/v2/flow/{token}`, `GET /login/v2/grant`, `POST /login/v2/apptoken` — all PHP-FPM
2. Add request/response tracing middleware (method, path, status, key headers, response time).
3. Return correct maintenance-mode behavior (503 + `X-Nextcloud-Maintenance-Mode: 1` + `Retry-After: 120`) and `/status.php` JSON from config/DB at startup.

### Verification steps
- `build/integration/features/maintenance-mode.feature`
- `build/integration/features/ocs-v1.feature`
- `build/integration/routing_features/apps-and-routes.feature`

---

## 2) Implement OCS envelope + auth behavior

### Coding steps
1. Implement OCS response wrappers for v1 and v2:
	- v1: HTTP 200 for most responses, OCS `statuscode=100` for success.
	- v2: preserve HTTP status mappings (`401`, `404`, `500`, etc.).
2. Implement format negotiation (`?format=` + `Accept` header) with XML default.
3. Implement unauthorized behavior with `WWW-Authenticate` and XHR `DummyBasic` variant.
4. Implement core OCS endpoints:
	- `/config` — returns `version: "1.7"` (not 1.8)
	- `/cloud/capabilities` — authenticated requests return full capabilities; unauthenticated return only `IPublicCapability` results; ETag = `md5(json_encode($result))`
	- `/person/check` — **cut:** forward to PHP-FPM via Phase 7 catch-all (ownCloud federation compatibility only; not called by web/mobile/desktop sync)
	- `/identityproof/key/{cloudId}` — **cut:** same reasoning; forward to PHP-FPM
5. Implement CSRF exceptions for OCS (`OCS-APIREQUEST: true`) and bearer-token bypass semantics.
6. Cache the capability payload: build it once at startup and after any `oc_appconfig` write that affects capabilities. Store as `Arc<RwLock<CapabilityCache>>` holding the pre-serialised XML and JSON blobs with their ETag. Capability probes (the most frequent unauthenticated request from sync clients) never touch the DB after the first build.

### Verification steps
1. Reuse existing OCS/integration suites:
	- `build/integration/features/ocs-v1.feature`
	- `build/integration/capabilities_features/capabilities.feature`
	- `build/integration/features/auth.feature`
2. Add assertion parity checks for:
	- OCS envelope fields (`status`, `statuscode`, `message`, `data`)
	- content type (`text/xml; charset=UTF-8`, `application/json; charset=utf-8`)
	- auth headers on 401.

---

## 3) Implement DAV service routing + auth stack

### Coding steps
1. Implement `remote.php` service mapping:
	- `webdav`, `dav`, `files`, `caldav`, `carddav`, `direct`.
2. Implement `public.php` DAV mapping (`webdav`, `dav`) and public-share auth flow.
3. Implement DAV auth layers:
	- Basic/session auth semantics (including CSRF rules: POST **always** requires CSRF check, even when DAV-authenticated).
	- Bearer token auth semantics: on failure return 401 with **no `WWW-Authenticate`** header (exception: send challenge for `mirall` UA when `oauth2.enable_oc_clients = true`).
	- Brute-force throttling and 429 behavior.
	- **Session cookie → uid resolution (requires Phase 7 FastCGI):** when no `Authorization` header is present but the `{instanceid}` cookie (PHP session cookie — named after `config.php`'s `instanceid`, NOT `nc_session_id`) or `nc_token` cookie exists, resolve the session to a uid via a FastCGI call to PHP. The `__session_resolve` shim endpoint bootstraps OC normally (session resumes from `{instanceid}` cookie), calls `OC::handleLogin()` which runs `tryTokenLogin()` (looks up PHP session ID in `oc_authtoken`) and `loginWithCookie()` (validates `nc_token` against `oc_preferences`). Cache results keyed on `SHA-256({instanceid}_cookie_value)` with 60-second TTL. For DAV routes, check `AUTHENTICATED_TO_DAV_BACKEND` — a **UID string** in `$_SESSION` (not a boolean; `Auth.php:91`) — per the 3-way check in `Auth.php:185-192`. SameSite strict cookie failure returns **412 Precondition Failed** (not 401 — `base.php:596`). The existing `session.rs` hardcodes `nc_session_id` as the trigger cookie name — must be fixed to use `{instanceid}` (§7.9.5). See PHASE-7.md §7.9 for full specification. This is the auth path used by the web browser Files app.
4. Implement the auth token hot cache — this is the highest-impact single cache in the system:
	- Structure: `Arc<RwLock<HashMap<[u8; 64], CachedToken>>>` keyed on the SHA-512 of the bearer value.
	- `CachedToken` holds: `uid`, `type`, `scope`, `expires`, `last_activity`, cached-at timestamp.
	- TTL: 5 minutes. On cache miss: query `oc_authtoken` by token hash, populate cache.
	- Invalidation: explicit eviction on token revocation or type change (wipe token).
	- Effect: every request from a desktop sync client — which reuses the same app token — becomes a hashmap lookup after the first hit. The `oc_authtoken` DB query disappears from the hot path.
5. Set DAV-specific headers and baseline hardening:
	- `Content-Security-Policy: default-src 'none';`
	- request/user tracing headers expected by clients.

### Verification steps
1. Reuse existing DAV auth/availability suites:
	- `build/integration/dav_features/webdav-related.feature`
	- `build/integration/dav_features/dav-v2.feature`
	- `build/integration/dav_features/dav-v2-public.feature`
	- `build/integration/features/auth.feature`
2. Validate no regressions in status codes (`401`, `207`, `201`, `204`, `403`) and key headers.

---

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
   - `{DAV:}sync-token` on collections (required for RFC 6578 delta sync REPORT requests).
3. Implement `SEARCH` request handling via `SearchDAV` library / `FileSearchBackend`:
   - Scope: `Directory` nodes, typically `/dav/files/{userId}`.
   - Supported filters: name, MIME type, size, last-modified, tags, file metadata.
   - Response: `207 Multi-Status`.
4. Implement RFC 6578 `sync-collection` REPORT support for delta sync (clients use this to fetch only changed resources since their last sync).
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

## 6) Files app HTTP APIs — Stretch Goal

> **Deferred:** All `/apps/files/api/v1/` REST and OCS files endpoints are forwarded to PHP-FPM via the Phase 7 catch-all as an interim measure. None are on the sync hot path that causes starvation. Implement natively in Rust after Phases 0–5 and 7 are complete.

### Coding steps
1. Implement REST endpoints from `apps/files/appinfo/routes.php`:
	- thumbnail endpoint
	- recent files
	- storage stats
	- view/config toggles
2. Implement OCS files endpoints:
	- direct editing info/open/create
	- templates list/create/path
	- transfer ownership endpoints.

### Verification steps
- Cypress: `cypress/e2e/files/*.cy.ts` (navigation, sorting, search, download, settings, recent view).
- Integration: `build/integration/files_features/*`

---

## 7) PHP app support via FastCGI dispatch

Rust is the sole HTTP server. For routes registered by Nextcloud apps (`files_sharing`, `comments`, `systemtags`, `federation`, `provisioning_api`, etc.) Rust dispatches the request to PHP-FPM over FastCGI. PHP apps run as a compute backend — not a second Nextcloud instance. There is no duplicate auth or routing logic in PHP.

### Coding steps
1. Embed a FastCGI client in the Rust server (crate: `fastcgi-client` or similar).
2. Build a PHP-FPM bootstrap shim that replaces the full `OC::handleRequest()` lifecycle:
   - Reads DB connection string and config from the same source as Rust.
   - Provides minimal OCP service stubs (`IRequest`, `IConfig`, `IDBConnection`, `IUserSession`) backed by the same DB.
   - Skips all PHP-side auth — Rust has already validated the session/token and injects the authenticated `userId` via FastCGI param `HTTP_X_NC_USER`.
3. **Secure the FastCGI trust boundary.** The PHP-FPM shim unconditionally trusts the injected `HTTP_X_NC_USER` param. This makes the FastCGI socket a privilege-escalation surface if reachable directly:
   - Bind PHP-FPM to a Unix socket owned by the Rust server process user, not a TCP port.
   - The shim must reject requests where `HTTP_X_NC_USER` is absent or empty (indicates a request that did not pass through Rust auth).
   - Document clearly: the FastCGI socket must never be exposed to the network or to untrusted local processes.
4. Build a route registry: on startup Rust scans `apps/*/appinfo/routes.php` (or a pre-generated manifest) and registers which URL prefixes map to PHP-FPM vs native Rust handlers.
5. For PHP-FPM-dispatched requests: forward original headers + body, inject `HTTP_X_NC_USER`, `HTTP_X_NC_SESSION_TOKEN`, return PHP response verbatim.
6. Implement a `__session_resolve` internal FastCGI endpoint in the PHP shim: receives the full `Cookie:` header via `HTTP_COOKIE` FastCGI param, bootstraps OC (`require base.php` → `OC::init()` resumes the PHP session from the `{instanceid}` cookie), calls `OC::handleLogin()` to resolve identity via the same auth chain PHP uses (`tryTokenLogin` → `loginWithCookie` → `tryBasicAuthLogin`), returns `{uid, dav_authenticated_uid}` as JSON plus any `Set-Cookie` headers from token rotation. See PHASE-7.md §7.9 for full specification.
7. Fix token hash: PHP uses `hash('sha512', $token . $secret)` (concatenation, `PublicKeyTokenProvider.php:414`), Rust uses `HMAC-SHA512(secret, token)` (different output). Replace `hmac_hash` in `bearer.rs` with concatenation hash. See PHASE-7.md §7.10.
8. Apps that need rewriting in Rust (to be decided): replace their PHP-FPM entry with a native Rust handler registered under the same route prefix.

### What stays in PHP-FPM (no rewrite needed)
- `files_sharing` OCS share/sharees API
- `provisioning_api` users/groups
- `comments`, `systemtags`, `federation`, `federatedfilesharing`
- Any other installed app

### Verification steps
- `build/integration/sharing_features/*.feature`
- `build/integration/features/auth.feature` (token forwarding to PHP-FPM)
- `build/integration/capabilities_features/capabilities.feature` (app capabilities still returned)
- `build/integration/features/provisioning-v1.feature`
- `build/integration/features/provisioning-v2.feature`

---

## 8) Load validation and starvation regression test

Caching is not deferred to this step — each cache is implemented in the step that owns its data (mime map in §0, capability cache in §2, token hot cache in §3). This step validates the system under the load conditions that motivated the rewrite.

### Coding steps
1. Build a synthetic load harness: N concurrent desktop sync clients, each running a tight loop of `GET /ocs/v2.php/cloud/capabilities` → `PROPFIND /dav/files/{user}` → random PUT/GET/DELETE. N should exceed the PHP-FPM worker count that would have caused starvation on the same hardware.
2. Add cache correctness regression checks:
	- Capability payload reflects a config change within one request of the write (no stale serve).
	- Token cache eviction on explicit revocation: revoke a token, confirm the next request with that token returns 401 without a DB round trip being required.
	- Mime-type cache remains consistent after a new mimetype is inserted.
3. Add concurrency regression checks for upload mutation paths:
	- Concurrent PUT to the same path: one wins, one gets a consistent response.
	- Chunked v2 assembly race: two MOVE requests for the same upload ID.

### Verification steps
1. Under the synthetic load at N > PHP-FPM worker ceiling: Rust server p99 latency remains flat; there is no request queue growth. This is the primary success criterion from the problem statement.
2. Re-run the full Behat + Cypress suite under load to confirm no cache-induced correctness regressions.
3. Benchmark representative client workflows end-to-end:
	- login + capabilities (should be near-zero DB cost in steady state)
	- WebDAV sync/upload/download
	- files UI actions (rename/move/delete/search).

---

## Existing tests you can directly reuse

### Integration (Behat/Gherkin) — highest value for protocol compatibility
- `build/integration/features/maintenance-mode.feature`
- `build/integration/features/ocs-v1.feature`
- `build/integration/features/auth.feature`
- `build/integration/features/provisioning-v1.feature`
- `build/integration/features/provisioning-v2.feature`
- `build/integration/capabilities_features/capabilities.feature`
- `build/integration/dav_features/dav-v2.feature`
- `build/integration/dav_features/dav-v2-public.feature`
- `build/integration/dav_features/webdav-related.feature`
- `build/integration/dav_features/principal-property-search.feature`
- `build/integration/files_features/checksums.feature`
- `build/integration/files_features/download.feature`
- `build/integration/files_features/metadata.feature`
- `build/integration/files_features/tags.feature`
- `build/integration/files_features/transfer-ownership.feature`
- `build/integration/ratelimiting_features/ratelimiting.feature`
- `build/integration/routing_features/apps-and-routes.feature`
- `build/integration/sharing_features/*.feature`

### UI/E2E (Cypress) — verifies real client behavior through files app
- `cypress/e2e/files/*.cy.ts`
- `cypress/e2e/core/*.cy.ts` (basic platform checks)

### Unit tests in PHP codebase (reference behavior)
- `apps/dav/tests/unit/**`
- `tests/Core/**`

Use these as behavior oracles while implementing Rust handlers; only write new tests where no existing suite covers the Rust-specific concurrency/state behavior.

---

## Future Considerations: Architectural Evolution

While the primary goal of this implementation is 1-to-1 parity with the PHP behavior via the `oc_filecache` database, future iterations should consider the "OCIS model" of metadata management to address long-term RDBMS bottlenecks.

### Recommendation: Metadata Write-Through Strategy
Do **not** move the source of truth away from the database yet (it would break compatibility with all 300+ PHP apps). Instead, implement a **Shadow Metadata Cache**:

1.  **Primary Authority:** Maintain `oc_filecache` as the source of truth for all write operations to ensure PHP compatibility.
2.  **Accelerator:** Store specialized metadata (ETags, checksums, permissions) in a high-performance sidecar format (e.g., Extended Attributes or a local Key-Value store like sled/RocksDB) within the Rust layer.
3.  **Read Path Optimization:** In `DavFileSystem`, prioritize the Shadow Cache for `PROPFIND` Depth-1 operations. Only fallback to SQL if the cache is stale or missing.

This provides the horizontal scalability benefits of ownCloud OCIS without the "Dual Source of Truth" corruption risks or the need for a total data migration.
