## 3) Implement DAV service routing + auth stack

### Coding steps
1. Implement `remote.php` service mapping:
	- `webdav`, `dav`, `files`, `caldav`, `carddav`, `direct`.
2. Implement `public.php` DAV mapping (`webdav`, `dav`) and public-share auth flow.
3. Implement DAV auth layers:
	- Basic/session auth semantics (including CSRF rules: POST **always** requires CSRF check, even when DAV-authenticated).
	- Bearer token auth semantics: on failure return 401 with **no `WWW-Authenticate`** header (exception: send challenge for `mirall` UA when `oauth2.enable_oc_clients = true`).
	- Brute-force throttling and 429 behavior.
	- **Session cookie → uid resolution (requires Phase 7 FastCGI):** when no `Authorization` header is present but the `{instanceid}` cookie (PHP session cookie — named after `config.php`'s `instanceid`, NOT `nc_session_id`) or `nc_token` cookie exists, resolve the session to a uid via a FastCGI call to PHP. The `__session_resolve` shim endpoint bootstraps OC normally (session resumes from `{instanceid}` cookie), calls `OC::handleLogin()` which runs `tryTokenLogin()` (looks up PHP session ID in `oc_authtoken`) and `loginWithCookie()` (validates `nc_token` against `oc_preferences`). Cache results keyed on `SHA-256({instanceid}_cookie_value)` with 60-second TTL. For DAV routes, check `AUTHENTICATED_TO_DAV_BACKEND` — a **UID string** in `$_SESSION` (not a boolean; `Auth.php:91`) — per the 3-way check in `Auth.php:185-192`. SameSite strict cookie failure returns **412 Precondition Failed** (not 401 — `base.php:596`). The existing `session.rs` hardcodes `nc_session_id` as the trigger cookie name — must be fixed to use `{instanceid}` (§7.9.5). See phase-7.md §7.9 for full specification. This is the auth path used by the web browser Files app.
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

---

Prev: [`05-implement-ocs-envelope-auth-behavior.md`](05-implement-ocs-envelope-auth-behavior.md) · Up: [`README.md`](README.md) · Next: [`07-implement-dav-files-tree-properties.md`](07-implement-dav-files-tree-properties.md)
