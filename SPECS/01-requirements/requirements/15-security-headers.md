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

---

Prev: [`14-special-dav-plugins.md`](14-special-dav-plugins.md) · Up: [`README.md`](README.md) · Next: [`16-caching-strategy.md`](16-caching-strategy.md)
