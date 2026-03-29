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
