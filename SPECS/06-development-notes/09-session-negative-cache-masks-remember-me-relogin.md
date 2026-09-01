# 09 — The session negative cache masked PHP's remember-me re-login, so a Rust-native DAV request after an idle gap got a 0 ms 401 and a Basic-auth prompt

**Status:** fixed (2026-09-02, pending deploy). **Related:** [Phase 15 · Wave 2.1 negative caching](../04-tasks/phase-15.md#21-session-resolution-exhaustion-f3--done-2026-08-10) (the F3 `SESSION_NEGATIVE_CACHE_TTL`), the `__session_resolve` shim ([`php-shim/index.php`](../../core-rs/php-shim/index.php)).

## Observable failure

After the site had not been accessed for a while, the web UI's first `PROPFIND /remote.php/dav/files/{uid}/` came back **401 with `WWW-Authenticate: Basic realm="Nextcloud"`** and a browser Basic-auth prompt, while OCS endpoints (`/ocs/v2.php/...`) and static files worked fine. A page refresh made the DAV request succeed.

Three HARs captured the asymmetry (`cloud2.home.lan_remote.php_dav_files_...02-54-06`, `...ocs_v2.php...02-54-44`, `...remote.php_dav_files...02-55-47`):

| request | result | signature |
|---|---|---|
| PROPFIND dav/files (02:54:06) | **401** | 12-byte body, no `x-user-id` / `x-debug-token` → served by Rust |
| GET ocs/v2.php recommendations (02:54:44) | 200 | `x-user-id` present → went to PHP |
| REPORT dav/files (02:55:47) | 207 | `x-user-id` / `x-debug-token` present → went to PHP |

The server journal pinned the exact failing request (local `01:32:06` = UTC `17:32:06`, matching the HAR):

```
nc_dav::handler: DAV: AuthInfo extension missing — returning 401
tower_http::trace::on_response: ... latency=0 ms status=401
```

`latency=0 ms` is the tell: a real `__session_resolve` round-trip to PHP-FPM costs hundreds of ms, so a 0 ms 401 means Rust short-circuited on an in-memory cache entry and never asked PHP. The same logs show DAV PROPFINDs at 1–5 ms (positive-cache hits) 26 s earlier, so the session was fine until something poisoned the cache.

## Root cause(s) — grounded

- **`session_auth` runs a `__session_resolve` on every request that carries a session cookie, regardless of whether Rust serves the request itself (`login=true`) or proxies it to PHP (`login=false`)** — [`middleware/auth.rs:564`](../../core-rs/crates/nc-server/src/middleware/auth.rs:564). On the proxied path the shim deliberately does **not** run `OC::handleLogin()` (`php-shim/index.php:619-633`): the real PHP request does that. But a failed read-only resolve was treated identically to a failed login.
- **Any `__session_resolve` returning `uid:null` was unconditionally written to the negative cache** (old `auth.rs:641`), whose TTL is `SESSION_NEGATIVE_CACHE_TTL = 5 s` ([`nc-auth/session.rs:42`](../../core-rs/crates/nc-auth/src/session.rs:42), introduced by commit `e95708f`, Phase 15 F3).
- **PHP's real request then re-logged-in via remember-me.** `OC::handleLogin()`'s remember-me branch needs all three of `nc_username`, `nc_token`, `nc_session_id` (`lib/base.php:1239-1242`) and restores the session + rotates the token. The negative cache masked exactly this recovery for its 5 s TTL.

The exact timeline (local time):

1. **01:32:02** — `GET /index.php/apps/files/` (proxied, `login=false`). Rust's read-only resolve found the expired PHP session → `uid:null` → **negative entry written**.
2. PHP's real request then ran `handleLogin()`, restored the session via remember-me → page loaded 200.
3. **01:32:06** — the page's first native DAV PROPFIND (`login=true`) hit the fresh negative entry → **0 ms 401** with a Basic-auth prompt.
4. 46 ms later the REPORT on the same path is proxied, so Rust forwards "anonymous" to PHP, which logs in itself → 207.

Why OCS works and a refresh fixes it: OCS is always proxied, so PHP's own login decides; and the refresh's page load rotates `nc_token`/session-id via `loginWithCookie()`, so the new cookie no longer matches the poisoned cache key and the next DAV resolve succeeds.

## Options weighed

- **A. Only negative-cache when the failure is definitive (`login=true`, or `login=false` with no remember-me cookies).** Chosen. Keeps the F3 anti-abuse guarantee for junk-cookie floods (a proxied resolve with no remember-me triple cannot recover either — PHP's branch needs all three cookies, `base.php:1239-1242`), while never masking the one case PHP can actually recover from.
- **B. Never negative-cache proxied (`login=false`) resolves.** Rejected: silently drops the F3 burst-absorption on exactly the surface an attacker would probe first (index.php/OCS with a junk session cookie). Only `login=false` + remember-me-present is non-definitive.
- **C. Longer/zeroed TTL knob.** Rejected: the TTL is the revocation knob by design (Phase 15 · 2.1); the bug is *whether* a failure is cached, not for how long.

## The choice

Gate the negative-cache write in `session_auth` behind a pure decision helper, `should_cache_negative_resolve` ([`middleware/auth.rs:698`](../../core-rs/crates/nc-server/src/middleware/auth.rs:698)):

```rust
fn should_cache_negative_resolve(login: bool, has_remember_me: bool) -> bool {
    login || !has_remember_me
}
```

with `has_remember_me_cookies` ([`nc-auth/session.rs:238`](../../core-rs/crates/nc-auth/src/session.rs:238)) checking that all three remember-me cookies are present. So:

- native resolve (`login=true`) failure → definitive → cache (unchanged);
- proxied resolve (`login=false`) without remember-me triple → PHP cannot recover either → cache (F3 preserved);
- proxied resolve with the triple → the real PHP request may re-login → **do not cache** (the fix).

## Verification

- New unit tests: `no_cache_negative_when_proxied_with_remember_me`, `cache_negative_when_native_resolve_fails`, `cache_negative_when_proxied_and_no_remember_me` (decision logic, no FPM needed) + `remember_me_present_with_all_three` / missing-any-one variants (cookie parsing).
- `cargo test -p nc-auth --lib` — 65 passed. `cargo test -p nc-server --bin nc-server -- middleware::` — 25 passed. `cargo build -p nc-server` clean.
- Live verification pending deploy: repeat the idle-gap scenario (leave the site untouched past session lifetime, then open the web UI) — the first DAV PROPFIND should now resolve against the re-logged-in session instead of 401-ing in 0 ms.

## Follow-ups

1. Deploy (`nc-server 0.1.0-2` or rebuilt package) and re-run the idle-gap repro; watch the journal for `AuthInfo extension missing` with `latency=0 ms`.
2. Consider a `warn!`/trace when a native resolve is skipped due to a *proxied-with-remember-me* miss — the current code path returns anonymous silently and the DAV handler 401s; visibility would have made this incident obvious in the logs.

Back: [`../README.md`](../README.md)
