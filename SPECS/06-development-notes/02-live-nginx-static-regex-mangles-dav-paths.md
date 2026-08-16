# 02 — Dav file paths were rewritten into the `/index.php` front-controller form by the live static-assets regex — every upload method 405'd on extension'd files

**Status:** fixed in the live nginx config (proposed 2026-08-16; applied by the operator — the config is operational, not in the repo). **Related:** [note 01](01-static-assets-outside-apps.md) (the static-asset locations this incident came from), [Phase 18](../04-tasks/phase-18.md) (the static-layer work).

### What happened

The iOS app failed to upload photos/videos. Every operation on a dav *file* path
ending in a static extension returned an empty-body **405**:

```
PROPFIND /remote.php/dav/files/<user>/Photos/2023/11/edbf0d10-….jpg   → 405, empty body
PROPFIND /remote.php/dav/files/<user>/Photos/2023/12/….mp4            → 405, empty body
```

Response fingerprint: `server: nginx`, `text/html; charset=UTF-8`, **zero bytes**,
fresh `oc_sessionPassphrase` / `__Host-nc_sameSiteCookielax` / session-id cookies,
CSP nonce, security headers (doubled by the backend + nginx `add_header`).
`nc-server` logged nothing — it never rejected the request.

### What we found out

1. **The live nginx static-assets location matched dav file paths.** Its regex
   `^/(?!index\.php/).*\.(?:css|js|…|jpg|png|…|mp4|webm)$` with
   `try_files $uri /index.php$request_uri` is extension-based across the whole
   webroot. A dav URL ending in `.jpg`/`.mp4` matched, the disk stat missed, and
   nginx internally redirected to `/index.php/remote.php/dav/files/…` — the
   PHP front-controller form.
2. **`location /` then proxied the mangled path to Rust** (`proxy_pass
   127.0.0.1:7000`), whose `/index.php/{*path}` route is a PHP-FPM fallback
   (`router.rs:454`).
3. **The fastcgi proxy split the URL correctly** — `derive_script_info`
   (`nc-fastcgi/src/lib.rs:645`) produced `SCRIPT_NAME=/index.php`,
   `PATH_INFO=/remote.php/dav/files/…` (verified in the param dump).
4. **PHP-FPM 405'd it.** The front-controller form of a dav URL has no Symfony
   route: `Router::match` throws `ResourceNotFoundException`, the catch is
   empty (`base.php:1155-1166`), and the request falls through to the WebDAV
   fallback which 405s any unmatched PROPFIND (`base.php:1168-1175`;
   `MethodNotAllowedException` → 405 at `base.php:1162`). php-fpm access log:
   `"PROPFIND /index.php" 405`.
5. **PHP never handles dav through the front controller in canonical setups
   either** — the official nginx template's php-location matches
   `/remote.php/dav/…` *directly* (`SCRIPT_FILENAME=remote.php`,
   `PATH_INFO=/dav/…`). The live config's `location /` → Rust makes the
   static regex the first regex location, which broke that invariant.
6. Reproduced on the dev stack (same binary): `PROPFIND
   /index.php/remote.php/dav/files/admin/…/x.jpg` → 405; the raw path → 404
   (routing fine, file absent).

### Options weighed

- **A. Explicit root exclusions** — `(?!index\.php/|remote\.php/|dav/|ocs/|public\.php/)`.
  Precise, but every root except `/dav/` contains `.php`; a future API script
  would silently regress.
- **B. Script/static dichotomy — `(?!dav/|.*\.php)`.** A path is never a static
  candidate if it is a script (contains `.php`) or the clean-URL dav root
  (`/dav/` — a script alias with no `.php`). Chosen.
- **C. Rust-side normalization** (strip the `/index.php` prefix and re-dispatch
  to the dav arbiter). Rejected: papers over the edge defect — the invariant is
  that the path is *never* rewritten, and PHP-faithful behavior is for the edge
  to keep raw paths (the canonical template never produces this form).

### The choice

B. It mirrors Rust's own static check — `try_static_files_check` skips any
path containing `.php` (`router.rs:164`) — covers future `.php` entry points by
construction, and was verified not to exclude any web-served asset (no served
file has `.php` in its filename). Both the static-assets and fonts locations
(`\.(?:otf|woff2?)$` — same flaw for `.otf`/`.woff2` uploads) got the lookahead.
After the change, dav paths fall through to `location /` → Rust raw, and the
arbiter serves the files tree natively (other subtrees reach PHP via the proxy
with the correct script split).

### Verification

- Dev-stack repro before: mangled path 405 / raw path routed fine.
- The fix is a two-location change to the live nginx config; applied by the
  operator (`nginx -t`, reload, re-run the app upload). Live post-fix
  confirmation pending at time of writing.

### Follow-ups — improvement backlog (upload-flow analysis, 2026-08-16)

Measurements first: the iOS client preflights every direct upload with
`PROPFIND depth:0` on the target path (conflict detection — it needs the
existing etag or absence before PUT). Preflight costs 28–89 ms live
(server-side share: auth + one filecache lookup); PUTs of 3–4 MB took 17–26 s
but the server writes 4 MB in ~200 ms locally (15–22 MB/s), so the PUT
wall-clock is uplink-bound (phone → mitmproxy → WireGuard), not server-bound.
The preflight is ~1% of the flow and client-mandated — do not chase it.

1. **Session cookie should win over Basic auth** (the real per-request win).
   The iOS client sends Basic + session cookies on every request. PHP's dav
   auth short-circuits on the session: `validateUserPass` returns true when
   the session is logged in, never touching Basic
   (`apps/dav/lib/Connector/Sabre/Auth.php:76-81`; same pattern in
   `ocs/v1.php:57-58`). Rust's `auth_check` dispatches on the Authorization
   header first (`auth.rs:293`) — Basic → throttle → token lookup → bcrypt
   `password_verify` on *every* request — the session-cookie path only runs
   when no Authorization header exists. So Rust pays ~50–150 ms of bcrypt per
   dav request where PHP pays nothing, and identity diverges (session user A +
   Basic user B: PHP → A, Rust → B). **Fix:** with Basic + cookies present,
   resolve the session first (cached `__session_resolve` path, 60 s TTL),
   skip Basic on success. Untouched at time of writing.
2. **`proxy_request_buffering off`** on the live `location /`. nginx defaults
   to spooling the whole upload to its temp dir before Rust sees a byte; the
   canonical template disables it for the same reason
   (`fastcgi_request_buffering off`). Removes the double-write; won't shrink a
   client's wall-clock when the uplink is the constraint.
3. **Upstream keepalive** — no `upstream { keepalive }` block, so each request
   opens a fresh TCP connection to Rust (the PROPFIND+PUT pair costs two
   handshakes). Minor on loopback; canonical pattern is `keepalive 32` +
   `proxy_set_header Connection ""`.
4. **`If-None-Match: *` on PUT** — already honored (sabre semantics; 412 when
   the file exists). Clients that use it skip the preflight entirely; iOS
   doesn't. No action.
