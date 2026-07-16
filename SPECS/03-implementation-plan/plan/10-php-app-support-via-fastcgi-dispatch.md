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
6. Implement a `__session_resolve` internal FastCGI endpoint in the PHP shim: receives the full `Cookie:` header via `HTTP_COOKIE` FastCGI param, bootstraps OC (`require base.php` → `OC::init()` resumes the PHP session from the `{instanceid}` cookie), calls `OC::handleLogin()` to resolve identity via the same auth chain PHP uses (`tryTokenLogin` → `loginWithCookie` → `tryBasicAuthLogin`), returns `{uid, dav_authenticated_uid}` as JSON plus any `Set-Cookie` headers from token rotation. See phase-7.md §7.9 for full specification.
7. Fix token hash: PHP uses `hash('sha512', $token . $secret)` (concatenation, `PublicKeyTokenProvider.php:414`), Rust uses `HMAC-SHA512(secret, token)` (different output). Replace `hmac_hash` in `bearer.rs` with concatenation hash. See phase-7.md §7.10.
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

---

Prev: [`09-files-app-http-apis-stretch-goal.md`](09-files-app-http-apis-stretch-goal.md) · Up: [`README.md`](README.md) · Next: [`11-load-validation-and-starvation-regression-test.md`](11-load-validation-and-starvation-regression-test.md)
