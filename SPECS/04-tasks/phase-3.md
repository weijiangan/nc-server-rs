# Phase 3 — Auth Stack

Goal: every auth method works correctly, the token hot cache is in place, and brute-force throttling is enforced.

---

### 3.1 Basic auth — password login
- [x] `Authorization: Basic {base64}` decoded; two-path verification (REQ §4.1/4.2):
  1. App token path: hash password field via HMAC-SHA512/SHA-512, query `oc_authtoken WHERE token = ? AND login_name = ?`, check expiry → returns `BasicAuthResult { uid, token_id, token_type }`
  2. Password path: bcrypt verify against `oc_users.password`
- [x] On success: `AuthInfo { uid, method: Basic, token_id }` attached to request extensions
- [x] On failure: record brute-force attempt; return `401` with correct `WWW-Authenticate`
- [x] 2FA gate applied after successful Basic auth (same as Bearer)

**Verify:** `build/integration/features/auth.feature` — Basic auth success and failure scenarios.

### 3.2 Bearer token auth
- [x] `Authorization: Bearer {token}` → SHA-512 hash (v1) or HMAC-SHA512 with server secret (v2) → lookup `oc_authtoken.token`
- [x] Brute-force `check_throttle` called **before** `lookup_bearer` (bearer tokens subject to same throttle as Basic)
- [x] On failure: record attempt; `401` with **no `WWW-Authenticate`** header
- [x] Exception: if `oauth2.enable_oc_clients = true` and UA contains `mirall` → send `WWW-Authenticate` challenge

**Verify:** `build/integration/features/auth.feature` — bearer token scenarios. Confirm no `WWW-Authenticate` on failure without mirall UA.

### 3.3 Token hot cache
- [x] Structure: `Arc<RwLock<HashMap<[u8; 64], CachedToken>>>` keyed on SHA-512 of bearer value
- [x] `CachedToken`: `uid`, `type`, `scope`, `expires`, `last_activity`, `cached_at`
- [x] TTL: 5 minutes. On miss: query `oc_authtoken`, populate cache
- [x] Explicit eviction: token revocation removes entry immediately
- [x] Cache is thread-safe: concurrent reads under `RwLock::read()` do not block each other

**Verify:** instrument with a DB query counter. Send the same bearer token 50 times consecutively; assert `oc_authtoken` SELECT count = 1 (first miss), not 50.

### 3.4 Token `last_activity` update
- [x] `oc_authtoken.last_activity` updated on each successful token auth
- [x] Update is async and non-blocking (fire-and-forget `tokio::spawn`); does not add latency to the request

**Verify:** auth with a token, then query `SELECT last_activity FROM oc_authtoken WHERE …`; assert timestamp is within 2 s of the request time.

### 3.5 Session cookie auth
- [x] `nc_session_id` cookie + SameSite guard cookies (`nc_sameSiteCookielax`, `nc_sameSiteCookiestrict`) — on HTTPS with path `/` the `__Host-` prefix is prepended to those two cookie names (see API_COMPATIBILITY.md §CSRF)
- [x] Strict cookie check (`passesStrictCookieCheck`): both `nc_sameSiteCookiestrict=true` and `nc_sameSiteCookielax=true` must be present; only triggered when `nc_session_id` or `nc_token` cookies are also present
- [x] Cookie check skipped entirely when neither `nc_session_id` nor `nc_token` are present (no cookies → no check)
- [x] `AUTHENTICATED_TO_DAV_BACKEND` session key check
- [x] Cookie check bypassed when `OCS-APIREQUEST: true` or `Authorization: Bearer …` present

**Verify:** `build/integration/features/auth.feature` — session cookie scenarios.

### 3.6 2FA enforcement gate
- [x] After successful credential auth (Bearer or Basic), query `oc_twofactor_providers` for the user
- [x] If a pending 2FA challenge exists (`enabled = 1`): return `401` with body `"Not Authenticated: 2FA challenge not passed."`
- [x] App tokens (`token_type = 1`) exempt from 2FA check
- [x] If no 2FA providers installed: skip check entirely (table may be empty)

**Verify:** integration test: user with a `oc_twofactor_providers` entry → `401`; user without → auth succeeds.

### 3.7 CSRF rules
- [x] GET, HEAD, OPTIONS: CSRF check skipped always
- [x] `OCS-APIREQUEST: true` header present: CSRF check skipped
- [x] Any `Authorization` header present: CSRF check skipped
- [x] Known sync client UA: CSRF check skipped — using exact `regex_lite` patterns from REQ §4.4:
  - Desktop: `^Mozilla/5\.0 \([A-Za-z ]+\) (?:mirall|csyncoC)/([^ ]*).*$`
  - Android: `^Mozilla/5\.0 \(Android\) (?:ownCloud|Nextcloud)-android/([^ ]*).*$`
  - iOS: `^Mozilla/5\.0 \(iOS\) (?:ownCloud|Nextcloud)-iOS/([^ ]*).*$`
- [x] POST from browser session: CSRF check always required regardless of DAV auth state
- [x] CSRF failure on POST: `userSession->logout()` semantics, then re-challenge

**Verify:** `build/integration/features/auth.feature` — CSRF scenarios.

### 3.8 Brute-force throttling
- [x] Failed login: `INSERT` into `oc_bruteforce_attempts`(action, occurred, ip, subnet, metadata) — `id` is omitted and auto-incremented by the DB; no manual ID generation
- [x] Delay formula: `min(25000ms, 100ms × 2^attempts)` applied as `tokio::time::sleep` before returning
- [x] Allowlist: skip throttle for IPs matching `oc_appconfig` CIDR entries (`bruteForce` app, `whitelist_*` keys); scanned with `values_with_prefix()` so non-contiguous key numbering (e.g. `whitelist_0`, `whitelist_5`) is handled correctly — no assumption of contiguous keys
- [x] Two-tier window (REQ §4.6): >N attempts within the last **30 minutes** → HTTP `429`; >N attempts within the last **12 hours** (but ≤30 min window) → sleep only, no 429; N configured via `oc_appconfig` key `auth.bruteforce.max-attempts` (default `10`)
- [x] HTTP `429` response includes `Retry-After` header indicating when the client may retry (API_COMPATIBILITY.md §Rate limiting)
- [x] `429` from OCS routes returns the OCS envelope (`ocs.meta.statuscode = 429`), not a bare 429 body; `index.php` non-HTML requests return `{"message": "…"}` JSON
- [x] Disabled when `auth.bruteforce.protection.enabled = false` in **`config.php`** (system config → `NcConfig.bruteforce_protection_enabled`); this is a system-level key, not an `oc_appconfig` entry

**Verify:** `build/integration/ratelimiting_features/ratelimiting.feature` — all throttle and 429 scenarios pass.
