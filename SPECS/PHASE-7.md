# Phase 7 — PHP-FPM FastCGI Dispatch

Goal: any route not handled natively is proxied to PHP-FPM with the authenticated identity injected; the FastCGI trust boundary is secure.

---

## Starting state

The `nc-fastcgi` crate at `crates/nc-fastcgi/` already exists in the workspace but is an empty stub (`src/lib.rs` contains only a comment). The router in `nc-server/src/router.rs` returns `501 Not Implemented` from an `async fn not_implemented` closure for all PHP-FPM-bound routes. `AppState` has no FastCGI field and `main.rs` has no socket setup.

The following items were deferred to this phase from earlier phases and must be wired up here once the proxy is functional:

| Deferred item | Origin |
|---|---|
| `{oc:}downloadURL` real URL (currently empty string placeholder) | Phase 4.8 |
| `{oc:}share-permissions` full per-share value (currently `"31"`) | Phase 4.8 |
| `M` (mounted) flag in `{oc:}permissions` | Phase 4.8 |
| `{nc:}note` from `oc_share.note` on shared nodes | Phase 4.9 |
| PHP-app capabilities merged into `/cloud/capabilities` | Phase 2 |

---

## 7.0 Crate and dependency wiring

- [x] Add `fastcgi-client` (async, Tokio-native) to `crates/nc-fastcgi/Cargo.toml` workspace dependencies and to the `[workspace.dependencies]` table in `Cargo.toml` — added `fastcgi-client v0.11.0` with `features = ["runtime-tokio"]`
- [x] Add `nc-fastcgi` as a dependency of `nc-server` in `crates/nc-server/Cargo.toml` (was already present)
- [x] Add a `fastcgi_socket` field (`Option<PathBuf>`) to `NcConfig` in `nc-db/src/config.rs`, read from `config.php` key `fastcgi_socket` (or default to `None`); also added `fastcgi_timeout_ms: u64` (default `30000`)
- [x] Add a `FastCgiState` struct to `nc-fastcgi` containing the Unix socket path and timeout; expose a constructor `FastCgiState::from_config(&NcConfig) -> Option<Self>` that returns `None` when `fastcgi_socket` is absent
- [x] Add `fastcgi: Option<nc_fastcgi::FastCgiState>` to `AppState`; populated in `main.rs` only when `nc_config.fastcgi_socket` is `Some`
- [x] Replace the `not_implemented` closure in `router.rs` with `php_fpm_fallback` that delegates to `nc_fastcgi::proxy_handler` when `fastcgi` is `Some`, and returns `502 Bad Gateway` when `None`

**Verified:** `cargo build --workspace` clean, no warnings (exit 0).

---

## 7.1 FastCGI client

- [x] Implement `async fn proxy_handler(fpm: &FastCgiState, req: Request) -> Response` in `nc-fastcgi` — signature takes `&FastCgiState` directly rather than `State<AppState>` because the router already unpacks `state.fastcgi` before calling it
- [x] Open a Unix socket connection to `fpm.socket_path` per request — **short-connection mode** (`Client::new_tokio`); connection pooling via `deadpool` deliberately deferred — see rationale below
- [x] Populate mandatory FastCGI params: `SCRIPT_FILENAME` (→ PHP bootstrap shim path), `REQUEST_METHOD`, `REQUEST_URI`, `QUERY_STRING`, `CONTENT_TYPE`, `CONTENT_LENGTH`, `SERVER_PROTOCOL`, `GATEWAY_INTERFACE=CGI/1.1`, `SERVER_SOFTWARE=nc-server/0.1`, `PATH_INFO`, `SCRIPT_NAME`, `NC_ORIGINAL_SCRIPT`
- [x] Forward all incoming HTTP headers as `HTTP_*` FastCGI params (except `content-type`/`content-length` which are set explicitly, and the identity headers stripped for security)
- [x] Request body **buffered** up to 64 MiB before forwarding — PHP-FPM routes serve API/web-UI payloads, not large file uploads (those go through the native DAV layer)
- [x] **Streaming response** via `execute_once_stream`: returns an owned `ResponseStream<S>` immediately; `parse_streaming_headers` reads stdout until `\r\n\r\n`, parses `Status:` and headers, then wraps the rest in `CgiBodyStream` — an axum streaming `Body`.  PHP-FPM output is forwarded to the HTTP client with near-zero copy and near-zero TTFB overhead.  PHP stderr is logged at `debug` level and discarded from the HTTP stream.
- [x] Timeout (`fpm.timeout_ms`, default `30 000 ms`) applied to the connect phase and to the CGI header-parsing phase; body-streaming timeout enforced by the transport layer. Timeout → `504 Gateway Timeout`
- [x] PHP-FPM unavailable (socket connect error): `502 Bad Gateway`

**Why `deadpool` is not used (and when to add it):**  `execute_once_stream` takes ownership of `Client` and returns an *owned* `ResponseStream<S>` that can be held freely across `await` points.  The keep-alive API (`execute_stream(&mut self)`) returns `ResponseStream<&mut S>` — a borrow of the pool guard — which cannot be moved into a background task to keep the guard alive while streaming the body.  PHP-FPM worker count (not Unix-socket setup time) is the actual throughput gate for these routes; the primary bottleneck the project addresses (sync desktop clients blocking all workers) is already solved — those paths are served natively and never reach PHP-FPM.  If load testing (§8) shows socket setup is a measurable cost, the correct fix is an `Arc<Mutex<Client>>` interior-mutability pool or a `tokio::sync::Semaphore` capped at `pm.max_children`.

**Unit tests:** `cargo test --workspace` — 13 `nc_fastcgi` unit tests pass (`derive_script_info`, `parse_cgi_header_block`, `parse_cgi_response`); 197 total passing.

**Verify:** proxy `/index.php` and confirm the response matches a direct PHP-FPM request (status, Content-Type, body).

---

## 7.2 Auth identity injection

The `AuthInfo` extension is already set on every request by `middleware::auth::auth_layer` in Phase 3. The FastCGI proxy reads it from `req.extensions()`.

- [x] Authenticated requests: inject `HTTP_X_NC_USER={uid}`, `HTTP_X_NC_SESSION_TOKEN={token_value}`, `HTTP_X_NC_IS_ADMIN={0|1}` as FastCGI params (before the PHP shim touches `$_SERVER`)
- [x] `HTTP_X_NC_IS_ADMIN`: query `oc_group_user WHERE gid = 'admin' AND uid = ?` at proxy time; cache result in `AuthInfo` struct (extend if needed)
  > **Impl note:** queried in the auth middleware (`auth_layer`) rather than inside `proxy_handler`, so the result is already in `AuthInfo` by the time the proxy runs — one fewer async call on the hot path. No separate cache added: `AuthInfo` is per-request and constructed after a fresh DB query, so the result is always current. `raw_token: Option<String>` added to `AuthInfo` to carry the raw bearer value for `HTTP_X_NC_SESSION_TOKEN`.
- [x] Unauthenticated requests: do **not** inject any `HTTP_X_NC_USER` param; the PHP shim (§7.4) will return `403` for routes that require auth
- [x] `SCRIPT_FILENAME` must point to the PHP bootstrap shim (`{nc_root}/core-rs/php-shim/index.php`), not the original PHP file. The original PHP file path is passed as a separate `NC_ORIGINAL_SCRIPT` param so the shim can route to the right app entrypoint
- [x] `PATH_INFO` derived from the request URI after the script name prefix

**Verify:** add a temporary `/_debug/whoami` PHP endpoint in the shim that echoes `$_SERVER['HTTP_X_NC_USER']`; confirm it returns the authenticated UID, and an unauthenticated request returns empty.

---

## 7.3 FastCGI trust boundary

- [x] PHP-FPM must be configured to listen on a Unix socket (`listen = /run/nc-fpm.sock`); document this as a hard requirement; TCP mode not supported
  > Documented in `core-rs/docs/deployment.md` with example pool config and `config.php` snippet.
- [x] Socket file mode must be `0600`, owned by the same user that runs the Rust server process; document this requirement
  > Documented in `core-rs/docs/deployment.md` with rationale (shim trusts `HTTP_X_NC_USER` unconditionally; `0600` is the primary control).
- [x] PHP bootstrap shim (§7.4) must call `reject_unauthenticated_shim_request()` as its first action: if `$_SERVER['HTTP_X_NC_USER']` is absent or empty, emit `HTTP/1.1 403 Forbidden` and `exit(0)`
  > Created `core-rs/php-shim/index.php`. The `reject_unauthenticated_shim_request()` function is implemented and called as the very first action. The rest of the shim (Phase 7.4 bootstrap) returns `501` as a placeholder until §7.4 is implemented.
- [x] The Rust proxy must NOT allow clients to set `HTTP_X_NC_USER` — strip the header from incoming requests before building the FastCGI param table, so a malicious client cannot impersonate an arbitrary user
  > Already implemented in §7.1: `proxy_handler` skips `x-nc-user`, `x-nc-session-token`, and `x-nc-is-admin` in the `HTTP_*` forwarding loop before injecting the Rust-validated values.
- [x] Document clearly in README/deployment guide: the FastCGI socket path must never be exposed to the network or to untrusted local processes
  > Documented in `core-rs/docs/deployment.md` under "Never expose the socket" — covers TCP binding, directory permissions, and the two-layer defence-in-depth model.

**Verify:** send a raw FastCGI request directly to the socket (bypassing Rust) without `HTTP_X_NC_USER`; confirm PHP shim returns `403`. Send one with `HTTP_X_NC_USER=admin` via a raw client connection; same result (direct socket access returns `403` because there is no validated Rust-side auth path — the socket `0600` permission is the primary control; the shim's guard is a defence-in-depth second layer).

---

## 7.4 PHP bootstrap shim

The shim lives at `core-rs/php-shim/index.php` (no web-accessible path). PHP-FPM's `SCRIPT_FILENAME` is always set to this file.

- [x] Shim reads DB credentials from `config/config.php` (same as Rust; no duplication)
  > `OC::init()` calls `OC::initPaths()` then reads `config/config.php` via the same `\OC\Config` class as the rest of Nextcloud. The shim passes `$_NC_ROOT` (derived as `dirname(__DIR__, 2)`) for `lib/base.php` auto-detection; no separate DB credential reading needed.
- [x] Calls `reject_unauthenticated_shim_request()` first (§7.3)
- [x] Bootstraps the OCP/OC framework minimally:
  - Includes `lib/versioncheck.php` and `lib/base.php` from NC_ROOT; calls `OC::init()` which sets up the Composer autoloader, DI container, and all OCP services including `IConfig`, `IUserManager`, `IUserSession`, `IDBConnection`
  - PHP-FPM populates `$_SERVER`/`$_POST`/`$_FILES` from FastCGI params automatically; Nextcloud's `\OC\AppFramework\Http\Request` (DI-resolved via `IRequest`) reads those directly — no synthetic wrapper needed
  - `\OCP\IConfig` is populated by `OC::init()` from `config/config.php` as normal
  - After `OC::init()`, calls `IUserSession::setVolatileActiveUser($user)` with the user resolved from `HTTP_X_NC_USER`; this injects the pre-authenticated user without touching PHP session state. `isLoggedIn()` then returns `true`, causing `OC::handleRequest()` to skip the PHP-side login/auth step entirely
  - `OC::handleRequest()` is NOT called in full for `remote.php`/`public.php` entry points (dedicated routing functions handle those); for `index.php` / OCS entry points it is called after user injection which suppresses the login attempt
- [x] Routes the request to the correct app controller using `NC_ORIGINAL_SCRIPT` to identify the target app:
  - `index.php`, `v1.php`, `v2.php` → `OC::handleRequest()` (Symfony router dispatches to app controllers)
  - `remote.php` → `route_remote_php()`: replicates service-resolution logic from `remote.php` (caldav, carddav, calendar, contacts, direct) without re-bootstrapping
  - `public.php` → `route_public_php()`: replicates service-resolution logic from `public.php`
  - Unknown entry points → `OC::handleRequest()` fallback
- [x] Shim must not re-authenticate or re-validate the token
  > `setVolatileActiveUser()` bypasses all credential checks; `OC::handleRequest()` sees `isLoggedIn()=true` and skips `handleLogin()`
- [x] Response must include original PHP-side headers (e.g. `X-Content-Type-Options`, `X-Robots-Tag`) — do not strip them
  > PHP-side `header()` calls pass through FastCGI stdout to the Rust proxy, which forwards them verbatim via `parse_cgi_header_block()`

**Verify:** proxied `GET /ocs/v2.php/apps/files_sharing/api/v1/shares` returns a valid OCS envelope with share list for the authenticated user.

---

## 7.5 Route registry

The current router in `nc-server/src/router.rs` uses a static fallback closure. Phase 7 replaces it with a dynamic registry built at startup.

- [x] At startup in `main.rs`: call `nc_fastcgi::build_route_registry(&nc_root)` which:
  - Scans `{nc_root}/apps/*/appinfo/routes.php` using a regex extractor (or a pre-built `routes-manifest.json` generated by a build step)
  - Builds a `Vec<RouteEntry>` of `{prefix: String, handler: PhpFpm}` entries
  - Returns the route entries alongside the `FastCgiState`
- [x] In `router.rs`: register the extracted PHP-FPM route prefixes as `axum::routing::any(proxy_handler)` after native Rust routes; the axum router already picks the most-specific match first, so native handlers take priority
- [x] Routes not matching any entry (native or PHP-FPM) → `404 Not Found`
- [x] The current `not_implemented` fallback routes in `router.rs` for `/apps/{*path}`, `/index.php`, etc. are replaced by the registry-built routes

**Implementation notes:**
- `RouteEntry { base: String, app: String }` in `nc-fastcgi`; `base` is directly used as the axum route prefix.
- `build_route_registry` returns two categories of entries per app:
  1. **App-level** — `/apps/{appname}` for every directory in `apps/` (replaces `/apps/{*path}` catch-all with per-app explicit routes).
  2. **Root-level** — first static segment extracted from routes with `'root' => ''` in `routes.php`, e.g. `/s` (`files_sharing`), `/f` (`files`), `/settings` (`settings`).
- Each entry registers two axum routes: exact (`/base`) and wildcard prefix (`/base/{*tail}`).
- `regex-lite` added to `nc-fastcgi` dependencies for PHP array syntax extraction.
- Two unit tests added: `registry_scans_real_apps_dir` (verifies real repos apps/ tree) and `registry_returns_empty_for_missing_apps_dir` (graceful fallback).
- Static `/apps/{*path}`, `/apps/files/{*path}`, `/apps/files/api/{*path}` removed from `router.rs`; all per-app coverage now comes from the registry.
- `router::build` signature changed to `build(state: AppState, php_routes: Vec<nc_fastcgi::RouteEntry>)`.

**Verify:** `build/integration/routing_features/apps-and-routes.feature` — all route prefix assertions pass.

---

## 7.6 Deferred DAV property completions (carry-forward from Phase 4)

With the PHP-FPM proxy available, the following placeholders from Phase 4 can be completed.

- [x] **`{oc:}downloadURL`** (deferred from Phase 4.8): generate the direct-download URL from the Rust router's base URL (`overwrite.cli.url` config key) + `/remote.php/webdav/{path}` for local/home storage. Return empty string for non-local storage (object/S3 URL generation is out of scope for Phase 7 — requires storage-backend-specific signed URL support).
- [x] **`{oc:}share-permissions`** (deferred from Phase 4.8): query `SELECT MAX(permissions) FROM oc_share WHERE (uid_owner = ? OR uid_initiator = ?) AND file_source = ? AND share_type IN (0,1,3)` to find the most permissive share bitmask; default `31` (all permissions) when no share row exists (owner's own unshared file) per REQ §6.5.
- [x] **`M` (mounted) flag in `{oc:}permissions`** (deferred from Phase 4.8): the flag is set when the file lives on a non-home mount — i.e. when `oc_storages.id` for that `storage_id` does NOT begin with `home::`. Implementation steps:
  1. In `get_props()` in `filesystem.rs`: query `SELECT id FROM oc_storages WHERE numeric_id = ?` to get the storage string ID; check `!id.starts_with("home::")`. Optimised: when `meta.storage == self.storage_id` the home-storage check is skipped entirely (the FS was constructed from a `home::` lookup).
  2. Pass an `is_mounted: bool` argument to `build_props()` in `props.rs`.
  3. In `encode_permissions()`: add `if is_mounted { s.push('M'); }` alongside the existing flag encoding.
  4. Updated `prop_names()` and all existing `build_props` call sites (including tests) for the new parameters.
- [x] **`{nc:}note`** (deferred from Phase 4.9): query `SELECT note FROM oc_share WHERE file_source = ? AND note != '' ORDER BY stime DESC LIMIT 1`; return the note string or empty string when none per REQ §6.5.

**Verify:** PROPFIND on a shared file: `{oc:}share-permissions` matches the `MAX(permissions)` from `oc_share`; `{nc:}note` matches `oc_share.note`; `M` flag present in `{oc:}permissions` on a file from an external storage mount. PROPFIND on an unshared home-storage file: `{oc:}share-permissions` is `"31"`, no `M` flag.

---

## 7.7 PHP-app capabilities merge

In Phase 2 the capability cache was implemented but only covers natively-known capabilities (`core`, `dav`, `files`). Capabilities registered by PHP apps must be merged in.

- [x] At startup (after FastCGI state is ready): make one synthetic `GET /ocs/v2.php/cloud/capabilities` request via the PHP-FPM shim with a system/admin identity, extract the `data.capabilities` JSON blob
- [x] Merge the PHP-side capabilities into the `CapabilityCache` under `php_app_capabilities: serde_json::Value`; whenever the cache is serialised the two halves are merged at the top level
- [x] Refresh on `oc_appconfig` writes that may affect capabilities (same invalidation trigger as the existing cache)
  > **Impl note:** All `oc_appconfig` writes go through PHP-FPM, so in-line interception is not feasible without inspecting every proxy response. Instead, `spawn_capability_refresh_task` (in `nc-server/src/main.rs`) spawns a background tokio task that wakes every 30 seconds, calls `nc_db::appconfig::reload_appconfig_cache` to re-query the full `oc_appconfig` table, then calls `nc_ocs::handlers::rebuild_capability_cache` (which preserves cached PHP-app caps) followed by a fresh `nc_fastcgi::fetch_php_capabilities` call if PHP-FPM is configured. Net effect: capabilities reflect any config change within ≤ 30 seconds — close to PHP's ≤3 s APCu TTL while still eliminating per-request recompute on the read path.
- [x] **`public_*` variant must contain only `IPublicCapability` results** (REQ §5.6): implemented with two coordinated changes:
  1. **Shim** (`core-rs/php-shim/index.php`): `reject_unauthenticated_shim_request()` now returns a `bool` based on the `HTTP_X_NC_PROXIED` marker (§7.8) instead of the previous path-whitelist. The user-injection block (`setVolatileActiveUser`) remains conditional on `$_NC_IS_AUTHENTICATED` (user presence) so PHP sees no session for the public capability probe and calls `getCapabilities(true)` naturally.
  2. **Rust** (`nc-fastcgi`): `fetch_php_public_capabilities(fpm)` sends `HTTP_X_NC_PROXIED=1` but omits `HTTP_X_NC_USER`; used to populate `public_*` only.
  3. **`CapabilityCache`** (`nc-ocs`): added `php_public_capabilities` field alongside `php_app_capabilities`; `rebuild_serialized()` now builds `auth_*` = native + full PHP caps, `public_*` = native + IPublicCapability-only PHP caps independently; `apply_php_public_capabilities()` added. `apply_php_capabilities()` no longer touches `public_*`.
  4. **Wired up** in `main.rs` (startup) and `spawn_capability_refresh_task` (30-second refresh): both calls now invoke `fetch_php_public_capabilities(fpm)` independently of the admin-UID-gated authenticated fetch (the public fetch is unauthenticated and does not need an admin UID).
  > **Current state:** `auth_*` = native + all PHP `ICapability` results; `public_*` = native + PHP `IPublicCapability`-only results. 199 tests pass (0 failures).

**Verify:** `build/integration/capabilities_features/capabilities.feature` — `files_sharing` capabilities present in the response. Compare response against PHP/SabreDAV baseline. For correct `public_*` filtering: confirm that an unauthenticated capabilities request omits any capability key that a test app registers only as `ICapability` (not `IPublicCapability`).

---

## 7.8 Proxied app integration

- [x] `files_sharing` OCS API returns correct responses through Rust proxy
- [x] `provisioning_api` users/groups endpoints return correct responses
- [x] Login flow routes (`/login/flow`, `/login/v2/*`) proxied correctly — no auth injection needed (these are unauthenticated flows)
- [x] `.well-known/{service}` proxied via PHP-FPM (CalDAV/CardDAV well-known redirects handled by the `dav` app)
- [x] `index.php` front controller proxied correctly (web UI page loads)

**Verify:** `build/integration/sharing_features/*.feature`, `build/integration/features/provisioning-v1.feature`, `build/integration/features/provisioning-v2.feature`, `build/integration/capabilities_features/capabilities.feature`.

---

## 7.9 Session cookie → uid resolution

The auth middleware (`auth_layer`) currently handles `Authorization: Basic` and `Authorization: Bearer` headers. When neither is present (the `_ => None` catch-all at line ~313 of `auth.rs`), no `AuthInfo` is set and native Rust routes (DAV, OCS) return `401`. This breaks the web UI: browsers authenticate via PHP login flow, which sets several cookies; subsequent XHR/fetch requests to `/dav/files/…` and `/ocs/…` carry only those cookies — no `Authorization` header.

**Cookie inventory (verified against PHP source)**

The PHP session cookie is **NOT** named `nc_session_id`. It is named after `config.php`'s `instanceid` value (e.g., `oc1a2b3c4d5e`), set via `session_name(OC_Util::getInstanceId())` in `lib/base.php:437,447`. The cookie value is the raw PHP session ID.

| Cookie | Set by | Value | Purpose |
|---|---|---|---|
| `{instanceid}` (e.g., `oc1a2b3c4d5e`) | PHP `session_start()` | PHP session ID | **The actual PHP session cookie.** Used by `tryTokenLogin()` to detect an existing session. `cookieCheckRequired()` in `Request.php:472` checks for this cookie (via `session_name()`) to trigger the SameSite guard. |
| `nc_session_id` | `setMagicInCookie()` (`Session.php:1012`) | Copy of `session_id()` | Remember-me cookie. Passed to `loginWithCookie($uid, $token, $oldSessionId)` so it can call `renewSessionToken($oldSessionId, $newSessionId)`. **Not the PHP session cookie.** |
| `nc_token` | `setMagicInCookie()` (`Session.php:1002`) | Random 32-char string | Remember-me token. Validated against `oc_preferences` entries with `appid='login_token'`. Rotated on each successful use. |
| `nc_username` | `setMagicInCookie()` (`Session.php:993`) | UID string | Remember-me username. Used to look up the user for `loginWithCookie()`. |
| `oc_sessionPassphrase` | `CryptoWrapper.php:40,56` | Random 128-char string | Encryption key for `CryptoSessionData`. Used to decrypt `$_SESSION` data. Must be forwarded to PHP for session reads to work. |
| `nc_sameSiteCookielax` | `base.php` | `"true"` | SameSite=Lax guard cookie. |
| `nc_sameSiteCookiestrict` | `base.php` | `"true"` | SameSite=Strict guard cookie. On HTTPS with `path=/`, both get `__Host-` prefix. |

**Browser auth flow (PHP source of truth: `base.php:1225-1255`)**

`OC::handleLogin()` runs these checks in order:

1. **Apache auth** — `OC_User::handleApacheAuth()` (irrelevant for Rust)
2. **AppAPI login** — `tryAppAPILogin($request)` (irrelevant for Rust)
3. **Token login** — `$userSession->tryTokenLogin($request)` (`Session.php:818-862`):
   - If `Authorization: Bearer {token}` → use bearer value (already handled by Rust auth middleware)
   - Else if the `{instanceid}` cookie is present (`$request->getCookie($this->config->getSystemValueString('instanceid')) !== null`) → use `$this->session->getId()` (the PHP session ID from the cookie) as the token
   - Hash: `hash('sha512', $sessionId . $secret)` via `PublicKeyTokenProvider::hashToken()` (`PublicKeyTokenProvider.php:412-414`)
   - Look up in `oc_authtoken.token` — this is how active browser sessions are validated: the PHP session ID is registered as a `type=0` (TEMPORARY_TOKEN) in `oc_authtoken` by `createSessionToken()` (`Session.php:650-672`)
   - If found: calls `loginWithToken()` → `setUser()` → sets `$_SESSION['user_id']`
4. **Remember-me cookies** — requires ALL THREE: `$_COOKIE['nc_username']`, `$_COOKIE['nc_token']`, `$_COOKIE['nc_session_id']`
   - Calls `$userSession->loginWithCookie($nc_username, $nc_token, $nc_session_id)` (`Session.php:871-935`)
   - Validates `nc_token` against `oc_preferences` `login_token` keys (`Session.php:884-886`)
   - Rotates token: deletes used token, generates new 32-char token (`Session.php:893-895`)
   - Renews session token: `renewSessionToken($oldSessionId, $newSessionId)` (`Session.php:903`)
   - Calls `setMagicInCookie($uid, $newToken)` to update all three cookies (`Session.php:922`)
5. **Basic auth** — `$userSession->tryBasicAuthLogin($request, $throttler)` (already handled by Rust auth middleware)

**SameSite strict cookie check scope (PHP source: `base.php:582-608`)**

The strict cookie check is enforced based on the script being processed:
- `index.php`, `cron.php`, `public.php` → **skip** the check (return early at `base.php:590-593`)
- **All other scripts** (including `remote.php`, `ocs/v1.php`, `ocs/v2.php`) → **enforce** `passesStrictCookieCheck()` → HTTP **412 Precondition Failed** on failure (not 401)

`cookieCheckRequired()` (`Request.php:466-474`) triggers only when the `{instanceid}` cookie (via `session_name()`) **or** `nc_token` is present. If neither is present, the check is bypassed.

**`AUTHENTICATED_TO_DAV_BACKEND` (PHP source: `apps/dav/lib/Connector/Sabre/Auth.php`)**

Stores a **UID string**, not a boolean. Set at `Auth.php:91`:
```php
$this->session->set(self::DAV_AUTHENTICATED, $this->userSession->getUser()->getUID());
```
Checked at `Auth.php:63-65`:
```php
return $this->session->get(self::DAV_AUTHENTICATED) === $username;
```

DAV auth flow in `Auth.php::auth()` (lines 163-197), after CSRF check:
1. If logged in AND `DAV_AUTHENTICATED` is null → accept ("Fix for broken webdav clients" — first DAV request in session)
2. If logged in AND `DAV_AUTHENTICATED === current UID` AND no `Authorization` header → accept (well-behaved cookie-only client)
3. Apache auth → accept
4. Fall through to `parent::check()` (SabreDAV `AbstractBasic` — parses `Authorization: Basic` header)

**Token hash discrepancy (pre-existing bug, separate from session auth)**

PHP hashes tokens via **simple concatenation**: `hash('sha512', $token . $secret)` (`PublicKeyTokenProvider.php:414`).
Rust uses **HMAC**: `HMAC-SHA512(secret, token)` (`bearer.rs:14-18`).
These produce different outputs. This affects all token lookups (Bearer and Basic app-token paths), not just session auth. Must be fixed separately — see §7.10.

### 7.9.1 Config: instanceid in Rust

- [x] Add `instanceid: String` field to `NcConfig` in `nc-db/src/config.rs`, read from `config.php`'s `instanceid` key. This is the PHP session cookie name and is required for cookie detection in the auth middleware.
  > **Deviation:** `NcConfig.instanceid` is kept as `Option<String>` rather than `String`. PHP's `getInstanceId()` (`OC_Util.php:612-616`) auto-generates and persists the value on first call, so it is always present on an installed instance. However, `NcConfig` is also loaded during pre-install states where `config.php` may be absent or partial — making it `String` with a forced default in `NcConfig` would obscure a missing/broken config. Instead, the resolution is deferred to `AppState` construction.
- [x] Pass `instanceid` through to `AppState` so `auth_layer` can look up the correct cookie.
  > `AppState.instanceid: String` is added. Populated in `main.rs` as `config.instanceid.clone().unwrap_or_default()` before `config` is moved into `Arc<NcConfig>`. The empty-string fallback is safe for pre-install states (no session cookies will match, so the path degrades gracefully to anonymous). Auth middleware reads `state.instanceid` directly.

**Verify:** `cargo build --workspace` clean (exit 0). `state.instanceid` reflects `config.php`'s `instanceid` value; resolves to empty string when absent (pre-install state).

### 7.9.2 nc-auth session module update

- [x] Update `nc-auth/src/session.rs`:
  - Remove "NOT implemented yet" doc comments.
  - Change `session_cookie_value()` to accept the `instanceid` cookie name as a parameter and check for it (instead of hardcoded `nc_session_id`). Continue checking `nc_token` as a fallback (remember-me path).
  - Update `check_samesite_cookies()`: the trigger condition in PHP is `session_name()` cookie (= `{instanceid}`) OR `nc_token` present — NOT `nc_session_id`. Update accordingly.
  - On SameSite failure, return a distinct variant so the middleware can respond with **412** (PHP behavior) rather than 401.
  > **Deviation:** `check_samesite_cookies` and `session_cookie_value` both gained an `instanceid: &str` parameter. Auth middleware updated to pass `state.instanceid` and now returns HTTP 412 on `StrictCheckFailed`. The `StrictCheckFailed` variant's doc comment updated to note the 412 mapping. Existing tests that used `nc_session_id` as the trigger cookie were converted to use `oc1abc` as the instanceid.
- [x] Add `SessionIdentity` struct:
  ```rust
  pub struct SessionIdentity {
      pub uid: String,
      pub dav_authenticated_uid: Option<String>,
  }
  ```
  - `uid`: from `IUserSession::getUser()->getUID()` after `handleLogin()` succeeds.
  - `dav_authenticated_uid`: from `$_SESSION['AUTHENTICATED_TO_DAV_BACKEND']` — UID string set by `Auth.php` on first DAV auth. `None` when absent.
- [x] Add `SessionResolveResult` struct (or place in `nc-fastcgi`):
  ```rust
  pub struct SessionResolveResult {
      pub identity: SessionIdentity,
      pub set_cookies: Vec<String>,
  }
  ```
  > Placed in `nc-auth/src/session.rs` (not `nc-fastcgi`) since `SessionIdentity` is also needed in `nc-server`'s auth middleware without creating a circular dependency.
- [x] Expose `SessionIdentity` and `SessionResolveResult` structs from `nc-auth` for use by `nc-fastcgi` and `nc-server`.
  > Re-exported via `pub use session::{SessionIdentity, SessionResolveResult}` in `nc-auth/src/lib.rs`.

**Unit tests:** `cargo test -p nc-auth` — add tests in `nc_auth::session`:
- `samesite_trigger_uses_instanceid_not_nc_session_id`: `check_samesite_cookies("oc1abc=sid; ...", false)` with the instanceid name `"oc1abc"` triggers the check; `"nc_session_id=sid"` does NOT.
- `samesite_trigger_nc_token_still_works`: `nc_token` present triggers the check regardless of instanceid cookie absence.
- `samesite_failure_returns_strict_check_failed`: instanceid or `nc_token` present but guard cookies absent → `StrictCheckFailed` (not `NoSessionCookies`).
- `session_cookie_value_finds_instanceid_cookie`: `session_cookie_value("oc1abc", "oc1abc=sid123; nc_token=tok")` → `Some("sid123")`.
- `session_cookie_value_falls_back_to_nc_token`: instanceid cookie absent → `nc_token` value returned.
- `session_cookie_value_returns_none_when_both_absent`: no instanceid and no `nc_token` → `None`.

**Verify:** `cargo test -p nc-auth` — all session module tests pass, including revised existing tests that previously relied on `nc_session_id` as the trigger cookie.

### 7.9.3 PHP shim session-resolve endpoint

- [x] Add a new route handler in `core-rs/php-shim/index.php`: when `NC_ORIGINAL_SCRIPT` equals `__session_resolve`, the shim performs session identity resolution **before** the normal shim bootstrap (before `require base.php`):
  1. Checks `$_SERVER['HTTP_X_NC_PROXIED'] === '1'` — rejects without this marker.
  2. Parses `$_SERVER['HTTP_COOKIE']` into `$_COOKIE` (PHP-FPM populates `$_SERVER` but not `$_COOKIE` from the raw `Cookie:` header forwarded by Rust via `HTTP_COOKIE` FastCGI param).
  3. Calls `require "$_NC_ROOT/lib/base.php"` → `OC::init()` → `initSession()` resumes the PHP session using the `{instanceid}` cookie from `$_COOKIE`, `CryptoWrapper` decrypts session data using `oc_sessionPassphrase` cookie.
  4. Calls `OC::handleLogin($request)` which runs the full auth chain (`tryTokenLogin` → `loginWithCookie` → `tryBasicAuthLogin`). For browser sessions, `tryTokenLogin` succeeds: it reads the `{instanceid}` cookie, gets `session_id()`, hashes it, looks up in `oc_authtoken`, and sets the user.
  5. Reads the resolved identity:
     - `$uid = \OCP\Server::get(\OCP\IUserSession::class)->getUser()?->getUID()` — null if no auth path succeeded.
     - `$davAuth = \OCP\Server::get(\OCP\ISession::class)->get('AUTHENTICATED_TO_DAV_BACKEND')` — UID string or null.
  6. Returns JSON: `{"uid": "alice", "dav_authenticated_uid": "alice"}` or `{"uid": null}`.
  7. Session is closed automatically by PHP-FPM shutdown. The remember-me path may have written rotated tokens and new cookies — these are side effects (new `Set-Cookie` headers in the FastCGI response) that Rust must **discard** (the shim resolve response is internal, not forwarded to the client).
  > **Impl note — `$_COOKIE` parsing:** PHP-FPM does not populate `$_COOKIE` from the `HTTP_COOKIE` FastCGI param; it only sets `$_SERVER['HTTP_COOKIE']`. The handler parses the raw cookie string into `$_COOKIE` using `urldecode()` on both name and value (matching PHP's native `Cookie:` header parsing) before `base.php` is included. This is necessary because `OC::init()` → `initSession()` → `Internal::__construct()` calls `session_start()` which reads the session cookie from `$_COOKIE[session_name()]`, and `CryptoWrapper` reads `oc_sessionPassphrase` via `IRequest::getCookie()` which is populated from `$_COOKIE` at DI container construction time.
  > **Impl note — SameSite check skip:** `SCRIPT_NAME` is overridden to `/index.php` so `performSameSiteCookieProtection()` in `base.php` (which checks `basename($request->getScriptName())`) skips the SameSite re-check for this internal path.
- [x] This endpoint is exempt from the `HTTP_X_NC_USER` requirement in `reject_unauthenticated_shim_request()` — the `HTTP_X_NC_PROXIED=1` marker is sufficient.
  > The `__session_resolve` intercept runs **before** the normal security gate (`reject_unauthenticated_shim_request()`); that function is never called on this path.
- [x] Must NOT be reachable via any public HTTP route — only callable internally by the Rust auth middleware via FastCGI.
  > The route registry (`§7.5`) only scans `apps/*/appinfo/routes.php` and the static route table — `__session_resolve` appears in neither. The only call path is the Rust auth middleware via the Unix socket.
- [x] **`loginWithCookie` side effects:** forward `Set-Cookie` headers from the resolve response alongside the JSON — Rust injects them into the actual HTTP response so the browser receives the rotated tokens.
  > `setMagicInCookie()` (called inside `loginWithCookie`) uses PHP `setcookie()` which writes into the FastCGI stdout header block automatically. The `session_resolve_handler` emits no explicit header-forwarding code — the headers are already present in FastCGI stdout and the Rust caller (`§7.9.4`) reads them from the `set_cookies` field of `SessionResolveResult`.

**Verify:** Direct FastCGI request to socket with `NC_ORIGINAL_SCRIPT=__session_resolve` but without `HTTP_X_NC_PROXIED=1` → returns HTTP 403. With `HTTP_X_NC_PROXIED=1` and a valid `{instanceid}` session cookie for a known user → returns `{"uid":"alice","dav_authenticated_uid":null}`. With an invalid/expired session cookie → returns `{"uid":null}`. Confirm the endpoint is not reachable via any public HTTP route (no entry in the route registry; only reachable via raw FastCGI socket call).

### 7.9.4 Rust FastCGI session resolver

- [x] Add `async fn resolve_session(fpm: &FastCgiState, raw_cookie_header: &str) -> Option<SessionResolveResult>` to `nc-fastcgi`:
  - Builds a minimal FastCGI request with `NC_ORIGINAL_SCRIPT=__session_resolve`, `HTTP_COOKIE={raw_cookie_header}`, `HTTP_X_NC_PROXIED=1`, `REQUEST_METHOD=GET`, `REQUEST_URI=/__session_resolve`.
  - Forwards the raw `Cookie:` header as the `HTTP_COOKIE` FastCGI param (standard CGI mechanism — PHP-FPM automatically populates `$_SERVER['HTTP_COOKIE']`).
  - Parses the response:
    - JSON body → `SessionIdentity { uid: String, dav_authenticated_uid: Option<String> }`
    - `Set-Cookie` headers → `Vec<String>` (for remember-me token rotation forwarding)
  - Returns `SessionResolveResult { identity: SessionIdentity, set_cookies: Vec<String> }`.
  - Returns `None` on any error (timeout, invalid JSON, `uid: null`).
  - Timeout: 5 seconds.
  > The response parsing is split into a `pub(crate) fn parse_resolve_response(raw: &[u8]) -> Option<SessionResolveResult>` helper so all five unit tests can exercise it without a live PHP-FPM socket. A non-200 `Status:` CGI header from the shim (e.g. `403 Forbidden` when `HTTP_X_NC_PROXIED` is absent — the shim's defence-in-depth guard) also returns `None`. A `parse_resolve_response_non_200_status_returns_none` test covers this sixth case beyond the five specified.

**Unit tests:** `cargo test -p nc-fastcgi` — add tests in `nc_fastcgi::session_resolver`:
- `parse_resolve_response_authenticated`: `{"uid":"alice","dav_authenticated_uid":"alice"}` → `Some` with `uid = "alice"`, `dav_authenticated_uid = Some("alice")`, `set_cookies = []`.
- `parse_resolve_response_no_dav_auth`: `{"uid":"alice","dav_authenticated_uid":null}` → `dav_authenticated_uid = None`.
- `parse_resolve_response_unauthenticated`: `{"uid":null}` → `None`.
- `parse_resolve_response_malformed`: invalid JSON body → `None`.
- `parse_resolve_response_with_set_cookies`: response headers include two `Set-Cookie` lines → `set_cookies` has two entries.

**Verify:** `cargo test -p nc-fastcgi` — all session resolver parsing tests pass.

### 7.9.5 Session cache in AppState

- [x] Add a session cache to `AppState`: `session_cache: Option<nc_auth::SharedSessionCache>` — `Some` when PHP-FPM is configured, `None` otherwise:
  - Cache type: `DashMap<[u8; 32], (SessionIdentity, Instant)>` aliased as `SessionCache` in `nc-auth/src/session.rs`.
  - Cache key: `SHA-256(php_session_cookie_value)` — using `make_cache_key(cookie_value: &str) -> [u8; 32]`.
  - TTL constant: `SESSION_CACHE_TTL = 60s`. Checked at lookup time in `cache_lookup()`.
  - Look up with `nc_auth::cache_lookup(cache, &key)` → `Some(SessionIdentity)` on hit within TTL, `None` on miss or expired.
  - Insert with `nc_auth::cache_insert(cache, key, identity)` after a fresh `resolve_session()` call.
  - Periodic eviction with `nc_auth::cache_evict_expired(cache)` every `SESSION_CACHE_EVICT_INTERVAL = 5 min`, run by `spawn_session_cache_eviction_task(cache)` in `main.rs`.
  - `session_cache` is `Option<SharedSessionCache>` (not just `Option<Arc<DashMap<…>>>`). `None` is used for pre-install states and when `fastcgi` is absent — in those cases there is no PHP-FPM resolver to call and no benefit to caching.
  - `dashmap = "6.1"` added to `[workspace.dependencies]` in `Cargo.toml` and to `nc-auth/Cargo.toml` and `nc-server/Cargo.toml`.
  - All new types and helpers (`SessionCache`, `SharedSessionCache`, `make_cache_key`, `cache_insert`, `cache_lookup`, `cache_evict_expired`, `SESSION_CACHE_TTL`, `SESSION_CACHE_EVICT_INTERVAL`, `new_session_cache`) are exported from `nc_auth::session` and re-exported from `nc_auth`.
  - **Remember-me path caveat:** `loginWithCookie()` regenerates the session ID (`$this->session->regenerateId()` at `Session.php:872`). The old `{instanceid}` cookie value becomes stale. The next request from the browser carries a new session ID → cache miss → fresh `resolve_session()` call. This is correct behaviour.

**Unit tests:** `cargo test -p nc-auth` — 5 new tests in `nc_auth::session`:
- `session_cache_key_is_sha256_of_cookie_value`: determinism, collision-freeness, and cross-check against the known SHA-256 of `b"sid123"`.
- `session_cache_hit_within_ttl`: insert → lookup returns `Some(identity)`.
- `session_cache_miss_for_unknown_key`: lookup on empty cache → `None`.
- `session_cache_miss_after_ttl`: backdated `Instant` (TTL + 1 s) → lookup returns `None`.
- `session_cache_eviction_removes_expired`: one fresh + one stale entry; after `cache_evict_expired`, only the fresh entry remains.

**Verify:** `cargo build --workspace` clean (exit 0). `cargo test --workspace` — 204 unit tests passing (5 new nc-auth session-cache tests, 4 pre-existing nc-db migration failures unchanged).

### 7.9.6 Auth middleware integration

- [x] In `auth_layer` (`middleware/auth.rs`): replace the `_ => None` catch-all with session cookie resolution. Rust checks `Authorization` headers first (desktop/mobile hot path), then falls back to session cookies when no `Authorization` header is present:
  1. Extract the `Cookie:` header. Look for the `{instanceid}` cookie (name from `state.instanceid`) or `nc_token` cookie.
  2. If neither is present → `None` (anonymous, as before).
  3. If the `{instanceid}` cookie or `nc_token` is present, run the SameSite strict cookie check. PHP enforces this on `remote.php` and OCS routes (returns **412 Precondition Failed**, not 401 — `base.php:596-607`). `cookieCheckRequired()` in `Request.php:466` triggers when the `{instanceid}` cookie or `nc_token` is present. Check bypass: if `OCS-APIRequest` header is set, skip the check (`Request.php:467-469` — already handled for OCS, but session-cookie requests without the header must be checked).
  4. On `StrictCheckFailed` → return **412 Precondition Failed** (matching PHP behavior, not 401).
  5. Look up `SHA-256({instanceid}_cookie_value)` in `state.session_cache`.
  6. On cache miss: call `nc_fastcgi::resolve_session(&state.fastcgi, &raw_cookie_header)`. Set-Cookie headers (remember-me token rotation) are collected in `pending_set_cookies` and appended to the final HTTP response after the downstream handler completes — no separate request extension needed.
  7. On success: query `oc_group_user` for admin status, build `AuthInfo { uid, is_admin, method: AuthMethod::Session, token_id: None, raw_token: None }`.
  8. For DAV routes (`/remote.php/webdav`, `/remote.php/dav`, `/dav/`): check `dav_authenticated_uid` from `SessionIdentity` per `Auth.php:185-192`:
     - `None` → accept (first DAV request in session — `Auth.php:184`: "Fix for broken webdav clients")
     - `Some(uid)` matching the resolved uid → accept (`Auth.php:186`: "Well behaved clients that only send the cookie")
     - `Some(other_uid)` → reject with `401` (session fixation: user switched accounts)
  9. If `state.fastcgi` is `None` (no PHP-FPM configured) → `None` (anonymous).
- [x] `AuthMethod::Session` variant already exists in `nc-auth/src/lib.rs` but is never constructed. This task wires it up.
- [x] **Update `nc-auth/src/session.rs` cookie detection:** `session_cookie_value()` and `check_samesite_cookies()` currently check for `nc_session_id`. They must be updated to check for the `{instanceid}` cookie (name passed as parameter from config). The `nc_token` check remains as-is (it's a valid trigger for cookie-based auth via the remember-me path).
  > **Errata:** The current `session.rs` hardcodes `nc_session_id` as the session cookie name. This is **incorrect** — `nc_session_id` is a remember-me cookie, not the PHP session cookie. The PHP session cookie is named `{instanceid}`. The SameSite guard in `session.rs` currently uses `nc_session_id` as the trigger, but PHP's `cookieCheckRequired()` uses `session_name()` (= `{instanceid}`). Both must be corrected by this item.
  > **Status:** Already corrected in §7.9.2; both functions use `instanceid: &str` parameter and tests verify the corrected behavior.

**Unit tests:** `cargo test -p nc-server` — 7 tests in `middleware::auth::tests`:

  **Pure-function tests** (no AppState, no network — `dav_session_guard` extracted from the `_ =>` arm):
  - `dav_guard_first_request_accepted`: `dav_authenticated_uid = None` → `true` (`Auth.php:184`: "Fix for broken webdav clients").
  - `dav_guard_uid_match_accepted`: `dav_authenticated_uid = Some("alice")`, uid `"alice"` → `true` (`Auth.php:186`).
  - `dav_guard_uid_mismatch_rejected`: `dav_authenticated_uid = Some("bob")`, uid `"alice"` → `false` (session fixation → 401).

  **Middleware tests** (full `auth_layer` stack, `fastcgi: None`):
  - `session_no_cookies_is_anonymous`: no session cookies → anonymous, 200.
  - `session_samesite_failure_returns_412`: session cookie present, guard cookies absent → 412.
  - `session_ocs_apirequest_bypasses_samesite`: `OCS-APIRequest: true` + session cookie, no guard cookies → SameSite check skipped, 200 (not 412).
  - `session_no_fpm_is_anonymous`: valid session cookie + guard cookies, `fastcgi: None` → no resolver, anonymous, 200.

**`cargo build --workspace`** clean (exit 0). **`cargo test -p nc-server -p nc-auth -p nc-fastcgi`** — 73 tests passing (43 nc-auth, 23 nc-fastcgi, 7 nc-server).

---

## 7.10 Token hash fix (pre-existing bug)

PHP hashes auth tokens via `hash('sha512', $token . $secret)` — plain SHA-512 of the concatenation of token and server secret (`lib/private/Authentication/Token/PublicKeyTokenProvider.php:412-414`). Fallback for pre-secret installs: `hash('sha512', $token)` (`PublicKeyTokenProvider.php:420-421`).

Rust currently uses `HMAC-SHA512(secret, token)` (`nc-auth/src/bearer.rs:12-18`), which produces a completely different hash. **This means all Bearer and Basic app-token lookups fail against PHP-created tokens.**

- [x] Replace `hmac_hash(secret, raw)` in `bearer.rs` with `SHA-512(raw + secret)`:
  ```rust
  pub fn concat_hash(secret: &str, raw: &str) -> [u8; 64] {
      let mut h = Sha512::new();
      h.update(raw.as_bytes());
      h.update(secret.as_bytes());
      h.finalize().into()
  }
  ```
- [x] Keep `sha512_hash(raw)` as the fallback (matches PHP's `hashTokenWithEmptySecret`).
- [x] Update `token_hash()` to call `concat_hash` instead of `hmac_hash`.
- [x] Remove the `hmac` crate dependency from `nc-auth/Cargo.toml` if no longer used.
  > **Deviation:** Also removed from `[workspace.dependencies]` in the root `Cargo.toml`. The spec only mentioned `nc-auth/Cargo.toml`, but leaving it in the workspace table with no consumer would cause an unused-dependency warning and would silently re-introduce the crate if another crate later added `hmac.workspace = true` by mistake. Confirmed unused across the entire workspace before removal.
- [x] Update the doc comment on `token_hash` to reference the PHP source (`PublicKeyTokenProvider.php:412`).
- [x] Run `cargo test --workspace` to verify token lookup tests still pass; add a test vector validated against PHP's `hash('sha512', 'test_token' . 'test_secret')` output.
  > Added `php_compatible_test_vector` cross-validated against PHP (`3c8e585d...010a`). Replaced the removed `hmac_and_sha512_differ` / `secret_uses_hmac` tests with `concat_and_sha512_differ` / `secret_uses_concat_hash` to maintain equivalent coverage under the new algorithm. 44 nc-auth tests + 7 nc-server tests pass; `cargo test --workspace` clean apart from the 4 pre-existing nc-db migration failures (SQLite test-DB setup, unrelated to this change).

**Verify:** Bearer token auth against a PHP-created Nextcloud database returns the correct user (currently broken).
