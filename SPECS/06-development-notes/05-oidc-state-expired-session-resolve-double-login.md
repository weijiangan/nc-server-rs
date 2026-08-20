# 05 — OIDC login failed with "The received state has expired" on the Rust vhost: `__session_resolve` ran the remember-me login twice per request, regenerating the session id (and rotating `nc_token`) between the OIDC start and the IdP callback

**Status:** fixed (commit `cbc6992`, 2026-08-21; redeploy pending at time of writing). **Related:** [note 02](02-live-nginx-static-regex-mangles-dav-paths.md) (the other live-vhost parity incident), [Phase 7 §7.9](../04-tasks/phase-7.md#79-session-cookie--uid-resolution) (the session-resolution design this incident comes from), commit `411d47b` (OCS DEBUG log gating, same session).

### What happened

On the live box (nginx → Rust `nc-server` at `cloud2.home.lan`), clicking "log in with SSO" (user_oidc → Kanidm) consistently produced:

> **Access forbidden — The received state has expired.**

The pure-PHP vhost (`cloud.home.lan`) worked; incognito worked; the normal browser profile failed. nc-server's journal showed the full flow in under a second — so it was not a real timeout:

```
GET /index.php/apps/user_oidc/login/1?…                  → 303   (to Kanidm; state stored in the PHP session)
GET /index.php/apps/user_oidc/code?code=…&state=…        → 403   (0.8 s later — state check failed)
```

### What we found out

1. **The state check "expires" whenever the session is empty.** user_oidc's `code()` reads `user_oidc.state-<state>` / `user_oidc.timestamp-<state>` from the session; a missing timestamp makes `time - null > LOGIN_FLOW_TIMEOUT (300)` true — "The received state has expired." (`apps/user_oidc/lib/Controller/LoginController.php:385-392`, const at `:87`). The user's callback came 0.8 s after `login/1`, so the state was gone, not old.
2. **The callback response replaced the session id.** The failing request's response carried `Set-Cookie: <session cookie>=<new id>` — PHP had started a session that did not hold the state.
3. **A curl replay with fresh cookies passed the state check on BOTH vhosts** (failing only later at the token endpoint, as expected for a dummy code). So session persistence through the Rust proxy works; the failure needed something the real browser had: a **valid remember-me trio** (`nc_username`/`nc_token`/`nc_session_id`).
4. **The root cause is the double login per request.** Rust's auth middleware resolves cookie-only requests via the shim's internal `__session_resolve` FastCGI round-trip (`middleware/auth.rs` `_ =>` arm → `nc_fastcgi::resolve_session`, `nc-fastcgi/src/lib.rs:1284`), which runs the **full PHP auth chain** — `OC::handleLogin()` (`php-shim/index.php` session_resolve_handler) — *before* the real request is processed. The real proxied request then runs `handleLogin` again via `base.php:1225-1255`. With a valid trio, `loginWithCookie()` (`lib/private/User/Session.php:871-935`) **regenerates the session id** (`session_regenerate_id(true)` — the old session file is deleted, `Session.php:872`) **and rotates `nc_token`**.
5. **Why PHP itself survives this but Rust does not.** PHP runs one `session_start` per request; its regeneration happens *inside* that request, after the state was loaded — `$_SESSION` stays in memory. Rust's design has **two** `session_start`s: the resolve's regeneration deletes the session file *before* the real request's `session_start`, which then starts empty. The state written by the real `login/1` request lives in a session id the browser no longer holds by the callback.
6. **Corroborating evidence.** The OCS capability polls (login-page `getCapabilities` every 30 s) alternated `loggedIn=1 userId=admin` / `loggedIn=0 userId=null` — the resolve login and the real-request login raced each other via the rotation. The phase-7 doc itself flags the trap: §7.9.3 step 7 says the resolve's side-effect cookies "must be discarded", but §7.9.6 forwards them, and §7.9.5 blesses the regeneration ("This is correct behaviour") — correct for per-request browsing, fatal for cross-request state like the OIDC flow.

### Options weighed

- **A. Never run `handleLogin` in the resolve (read-only always).** Simplest; the real request restores remember-me exactly as PHP does. Rejected as a full fix: Rust-served DAV paths have *no* real PHP request — the resolve is their only login opportunity (mirroring PHP's `remote.php`), so dead-session + valid-trio DAV clients would start getting 401s.
- **B. Keep the resolve login but suppress its side effects (skip regeneration, discard rotation cookies).** Fragile: `loginWithCookie` already rotated `nc_token` in `oc_preferences`; discarding the Set-Cookie would invalidate the browser's trio without telling it.
- **C. Have Rust skip the resolve for requests it proxies anyway.** Rust needs the resolved uid for OCS identity injection (`HTTP_X_NC_USER`) on proxied paths — removing the resolve breaks that.
- **D. Forward the resolve's regenerated session id into the real request (rewrite the Cookie header).** The callback's *own* resolve would regenerate again between requests — cross-request state cannot survive any per-request regeneration. Fundamentally wrong shape.

### The choice

**B', gated on who serves the request.** Rust passes `NC_SESSION_RESOLVE_LOGIN=1` only when it serves the request itself; the shim runs `OC::handleLogin()` only then. The classification is the arbiter's own, extracted to a shared pure function `router::dav_served_by_rust(path, method)` (native files tree + `/uploads` + `/bulk` POST; everything else — `PROXIED_DAV_SUBTREES`, SEARCH/REPORT, dav root, app collections, non-dav paths — proxied) so dispatch and auth cannot drift. Proxied paths get a read-only resolve; the real request's single `handleLogin` performs the remember-me login exactly once — byte-for-byte the PHP request shape, which is why `cloud.home.lan` always worked.

### Verification

- Diagnosis evidence: journal 303→403 in 0.8 s; session-id replacement in the callback response; curl replay (fresh cookies) passing the state check on both vhosts; incognito-works / profile-fails asymmetry; alternating `loggedIn=1/0` polls.
- `cargo test -p nc-server -p nc-fastcgi -p nc-auth` — 151 tests pass, including 3 new `dav_served_by_rust` classification tests (files tree native; uploads/bulk native; subtrees/SEARCH/REPORT/root/app-collections/webdav proxied).
- Live verification after redeploy (pending at time of writing): OIDC login on `cloud2.home.lan`, and the capability polls should stop alternating.

### Follow-ups

1. **Phase-7 doc vs implementation divergence** — §7.9.3 step 7 ("Rust must discard the side-effect cookies") vs §7.9.6 step 6 (forward them) and §7.9.5 (regeneration "correct behaviour"). The gating resolves the proxied case; the doc's §7.9.5 caveat deserves a note that per-request regeneration is only safe when no cross-request session state exists.
2. **The live box's OCS DEBUG flood** — the shim's `error_log("OCS DEBUG: …")` wrote on every OCS request (494 MB log); commented out by hand on the live box at 02:59 on 2026-08-21. Now gated on the `debug` config (commit `411d47b`) — the manual edit becomes obsolete on redeploy.
3. **DAV remember-me restore edge** — preserved for Rust-served paths (resolve login still on); for proxied paths the real request restores — full PHP parity either way.
