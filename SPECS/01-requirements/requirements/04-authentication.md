## 4. Authentication

### 4.1 Authentication methods

| Method | Trigger | Notes |
|---|---|---|
| Basic (password) | `Authorization: Basic …` header | Password or app token |
| Bearer token | `Authorization: Bearer …` header | App token / OAuth2 token |
| Session cookie | `{instanceid}` cookie (PHP session) + SameSite guard cookies | Web browser flow (see §15.2 for cookie names). The PHP session cookie is named after `config.php`'s `instanceid` value (e.g., `oc1a2b3c4d5e`), set via `session_name(OC_Util::getInstanceId())` in `lib/base.php:437,447`. **Not** `nc_session_id` — that is a separate remember-me cookie. |
| Remember-me cookies | `nc_token` + `nc_username` + `nc_session_id` cookies | Used by `loginWithCookie()` when the PHP session has expired but remember-me cookies remain. All three must be present. `nc_token` is validated against `oc_preferences` `login_token` entries, rotated on each use. `nc_session_id` stores the old `session_id()` for `renewSessionToken()`. |
| Password-less token | Basic header, token has `passwordless = true` | No password login permitted |

### 4.2 Token storage (`oc_authtoken`)

Each app password / device token is a row in `oc_authtoken` with:
- `uid` (owner user)
- `login_name`
- `password` (encrypted password for token refresh — NOT bcrypt; encrypted with the token itself as key)
- `name` (device/client label)
- `token` (hashed session token — see hashing note below)
- `type` (`0` = temporary session, `1` = permanent app token, `2` = wipe token)
- `last_activity` (unix timestamp, updated per request)
- `last_check` (timestamp for periodic password re-validation)
- `scope` (JSON; lockdown scopes such as filesystem-only)
- `expires` (optional expiry timestamp)
- `private_key` / `public_key` (end-to-end encryption keys)

**Token hash algorithm** (source: `lib/private/Authentication/Token/PublicKeyTokenProvider.php:412-421`):
- Primary: `hash('sha512', $token . $secret)` — SHA-512 of the **concatenation** of raw token value + server secret from `config.php`'s `secret` key. This is NOT plain SHA-512 and NOT HMAC.
- Fallback (pre-NC 20 installs without a secret): `hash('sha512', $token)` — plain SHA-512 without the secret suffix.

Token lookup on each request: compute `SHA-512(raw_token || server_secret)`, query `oc_authtoken.token`.

### 4.3 DAV authentication precedence

**Pre-auth phase** (runs before `Auth.php`): `OC::handleLogin()` in `lib/base.php:1225-1255` establishes the logged-in state via these checks in order:
1. Apache auth (`OC_User::handleApacheAuth()`)
2. Token login (`$userSession->tryTokenLogin($request)` — `Session.php:818-862`):
   - If `Authorization: Bearer {token}` → use bearer value as the token
   - Else if the `{instanceid}` cookie is present → use `$this->session->getId()` (the PHP session ID) as the token
   - Hash the token via `hash('sha512', $token . $secret)`, look up in `oc_authtoken`
   - If found: `loginWithToken()` → `setUser()` → sets `$_SESSION['user_id']`
   - Browser sessions create a `type=0` (TEMPORARY_TOKEN) row in `oc_authtoken` via `createSessionToken()` at login time, so the PHP session ID is a valid token
3. Remember-me cookies: requires ALL THREE `$_COOKIE['nc_username']`, `$_COOKIE['nc_token']`, `$_COOKIE['nc_session_id']` → `loginWithCookie($uid, $token, $oldSessionId)` (`Session.php:871-935`)
4. Basic auth (`$userSession->tryBasicAuthLogin($request, $throttler)`)

**DAV auth phase** (`Auth.php::auth()` — `apps/dav/lib/Connector/Sabre/Auth.php:163-197`): runs after `handleLogin()` has (possibly) established a session.
1. CSRF check first (`requiresCSRFCheck()` — see §4.4). POST CSRF failure → forced logout + re-challenge.
2. 2FA check: if `twoFactorManager->needsSecondFactor()` → `401 "2FA challenge not passed."`
3. Session shortcuts (checked in this order):
   - Logged in AND `AUTHENTICATED_TO_DAV_BACKEND` is `null` → accept ("Fix for broken webdav clients" — first DAV request in session, `Auth.php:186`)
   - Logged in AND `AUTHENTICATED_TO_DAV_BACKEND === current UID` AND no `Authorization` header → accept ("Well behaved clients that only send the cookie", `Auth.php:188`)
   - Apache auth → accept
4. Fall through to `parent::check()` (SabreDAV `AbstractBasic::check()` — parses `Authorization: Basic` header, calls `validateUserPass()` which calls `logClientIn()` and sets `AUTHENTICATED_TO_DAV_BACKEND` on success, `Auth.php:91`)
5. Bearer token failure: return HTTP 401 **with no `WWW-Authenticate` header** (unlike Basic auth). Exception: if `oauth2.enable_oc_clients = true` in config and the `User-Agent` contains `mirall`, send a standard `WWW-Authenticate` challenge.

**`AUTHENTICATED_TO_DAV_BACKEND`** stores a **UID string** (not a boolean). Set at `Auth.php:91`: `$this->session->set(self::DAV_AUTHENTICATED, $this->userSession->getUser()->getUID())`. Checked at `Auth.php:63-65`: `$this->session->get(self::DAV_AUTHENTICATED) === $username`. This prevents session fixation when a WebDAV client resends cookies after an account change.

**Response headers on auth failure:**
4. If the request is from an XMLHttpRequest (`X-Requested-With: XMLHttpRequest`) and Basic auth fails → respond with `WWW-Authenticate: DummyBasic realm="…"` (prevents browser pop-up).
5. If not an XMLHttpRequest and Basic auth fails → respond with `WWW-Authenticate: Basic realm="Nextcloud"`.

### 4.4 CSRF checks (DAV context)

CSRF check is **skipped** when:
- Request method is GET, HEAD, or OPTIONS
- `User-Agent` matches Nextcloud desktop, Android, or iOS client patterns (exact regex from `IRequest.php`):
  - Desktop: `/^Mozilla\/5\.0 \([A-Za-z ]+\) (?:mirall|csyncoC)\/([^ ]*).*$/`
  - Android: `/^Mozilla\/5\.0 \(Android\) (?:ownCloud|Nextcloud)\-android\/([^ ]*).*$/`
  - iOS: `/^Mozilla\/5\.0 \(iOS\) (?:ownCloud|Nextcloud)\-iOS\/([^ ]*).*$/`
- User is not logged in
- Request method is **not** POST, and user is logged in and already DAV-authenticated (`AUTHENTICATED_TO_DAV_BACKEND` set)

CSRF check **always required** for POST requests from browser sessions, **regardless** of DAV authenticated state. On a POST CSRF failure, the session is forcibly logged out and the request is re-challenged (not a plain 401).

### 4.5 2FA enforcement

If the authenticated user has a pending 2FA challenge (`oc_twofactor_providers` — only relevant when 2FA app is installed via PHP-FPM), DAV authentication must return `401 Not Authenticated: 2FA challenge not passed.`

### 4.6 Brute-force throttling

- Table: `oc_bruteforce_attempts` (columns: `action`, `occurred`, `ip`, `subnet`, `metadata`)
- On each failed login: `INSERT` a row for action `login`, IP and /24 subnet.
- On each subsequent login attempt: compute delay = `min(25s, 100ms * 2^attempts)` and sleep.
  - `firstDelay = 0.1` (100 ms), formula: `delay = firstDelay * 2^attempts`.
  - Maximum delay cap: **25 seconds** (`IThrottler::MAX_DELAY = 25`, `MAX_DELAY_MS = 25000`).
- 429 trigger: when `attempts > auth.bruteforce.max-attempts` (default **10**, configurable) **and** those same attempts occurred within the last **30 minutes** → throw `MaxDelayReached` → HTTP 429. A bare attempt count over the threshold in the last 12 hours only throttles (sleeps), not 429.
- Allowlist stored as `oc_appconfig` entries: `appid = 'bruteForce'`, config keys prefixed `whitelist_`, values are IP CIDR range strings. Bypass check done before delay calculation.
- Disable if system config `auth.bruteforce.protection.enabled = false`.

---

---

Prev: [`03-status-php.md`](03-status-php.md) · Up: [`README.md`](README.md) · Next: [`05-ocs-api.md`](05-ocs-api.md)
