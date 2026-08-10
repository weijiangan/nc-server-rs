# 10) Edge Security Hardening

Status: proposed. Source: security reassessment of the PHP-FPM forwarding path (2026-07-26), covering `nc-fastcgi`, the PHP bootstrap shim, `nc-server` routing/middleware, and `nc-auth` session handling, cross-checked against the PHP reference in `workspace/server/`.

---

## Verdict

**Keep the architecture. Finish the edge.**

The split — Rust owns byte-streaming hot paths, PHP-FPM keeps the long tail of app logic behind a bootstrap shim — is the right one, and the trust core holds up under adversarial review:

- The channel-trust marker (`HTTP_X_NC_PROXIED`) cannot be spoofed through the proxy: `fastcgi-client` 0.11.0 `Params` is a `HashMap` and `custom()` is `insert` (`~/.cargo/registry/.../fastcgi-client-0.11.0/src/params.rs:35,45`), so the proxy's post-header-loop injection (`nc-fastcgi/src/lib.rs:266-275`) deterministically wins over any client-supplied value, and duplicates never reach the wire.
- Identity params (`HTTP_X_NC_USER`, `HTTP_X_NC_IS_ADMIN`, `HTTP_X_NC_SESSION_TOKEN`) are injected only after Rust-side brute-force, token, and 2FA checks (`nc-server/src/middleware/auth.rs:135-311`), and the client-supplied versions are stripped (`lib.rs:230-239`).
- `SCRIPT_FILENAME` is always the shim; the entry-point dispatch is a fixed map (`php-shim/index.php:121-163`). No URI construction can make PHP `require` a client-chosen file.
- The DAV session-fixation guard mirrors `apps/dav/lib/Connector/Sabre/Auth.php:184-186` exactly (`auth.rs:484-489`); session cache TTL is 60 s (`nc-auth/src/session.rs:31`), bounding revocation lag; `is_admin` is re-queried per request, not cached.

Every serious finding is at the **edge**, not in the trust core: missing canonical webserver protections, missing proxy hygiene, missing resource governance. These are the responsibilities nginx/apache gave the canonical stack for free, and this plan treats them as the remaining work of taking the forwarding path seriously as **permanent production infrastructure** — because third-party PHP apps guarantee it is permanent, not scaffolding.

The one genuinely structural risk is long-term **drift between the two auth stacks** (Rust reimplements PHP's auth semantics; every divergence is a latent bug — the XFF handling in finding F2 is the first confirmed instance). The plan addresses this procedurally: port PHP algorithms line-for-line instead of paraphrasing, and build a differential parity corpus that turns drift into a red build.

---

## Findings

Severity and anchors as assessed. "PHP reference" is the behavior the canonical stack has and we lack.

| # | Finding | Sev | Location | PHP / canonical reference |
|---|---|---|---|---|
| F1 | `try_static_files` serves any regular file under `nc_root` unauthenticated, with no deny list. With `datadirectory` inside the NC root (the default: `nc-server/src/state.rs:81-83`) this exposes `/data/**` — anonymous download of all user files. Universally exposes `/3rdparty/**`, `/lib/**` non-php, `/templates/`, `/occ`, dotfiles, `/.git/**` in dev checkouts. | CRITICAL | `nc-server/src/router.rs:36-59` | `config/.htaccess` = `Require all denied`; `data/.htaccess` written at install by `Setup::protectDataDirectory()` (`lib/private/Setup.php:632-660`, same deny-all); current Nextcloud nginx admin manual blocks `location ~ ^/(?:build\|tests\|config\|lib\|3rdparty\|templates\|data)(?:$\|/)` and `location ~ ^/(?:\.\|autotest\|occ\|issue\|indie\|db_\|console)` — both with `return 404` |
| F2 | Client IP from leftmost `X-Forwarded-For` with no trusted-proxy gate; `X-Forwarded-Proto/Host/Port` honored unconditionally. Brute-force throttle keys (`auth.rs:492-500`) and the `REMOTE_ADDR` handed to PHP (`lib.rs:163-212`) are attacker-controlled. The documented nginx config (`docs/deployment.md:118`) appends to client-supplied XFF, so the recommended deployment is spoofable. Related gap: native routes do not enforce `trusted_domains` at all (PHP enforces it for every request it boots — see Wave 1.2). | HIGH | `auth.rs:492-500`; `lib.rs:163-197` | `lib/private/AppFramework/Http/Request.php`: `getRemoteAddress()` (571-611) honors XFF **only** when `REMOTE_ADDR ∈ trusted_proxies` (CIDR via `IpUtils::checkIp`, 553-562), walks entries right-to-left skipping trusted ones; `getServerProtocol()` (630-670) gates `X-Forwarded-Proto` on `fromTrustedProxy()`; `getInsecureServerHost()` (799-821) gates `overwritehost`/`X-Forwarded-Host` on `fromTrustedProxy()`. Enforcement: `lib/base.php:872-912` — untrusted host + `installed` → 400 `{"error": "Trusted domain error.", "code": 15}` for `/status.php`, 400 + `core/untrustedDomain` guest page otherwise, `/css/*` pathinfo exempt |
| F3 | Unauthenticated PHP-FPM exhaustion: a request with `{instanceid}=junk` + both SameSite guard cookies passes the cookie check, misses the session cache, and triggers a full `OC::init` + `OC::handleLogin` bootstrap with a 5 s budget (`auth.rs:315-367`, `lib.rs:1138+`). Failures are **not** cached; no concurrency cap. Fires on native routes too (every DAV PROPFIND with junk cookies). ~`pm.max_children` parallel requests saturate FPM → all PHP-routed traffic 502/504. | MEDIUM | `auth.rs:315-367`; `lib.rs:1138-1245` | PHP has the same per-request bootstrap cost, but there PHP *is* the handler; Rust adds the bootstrap on top of native handling and retries it on every uncached failure |
| F4 | Request body buffered up to 64 MiB per request with no global in-flight cap — N parallel POSTs = N×64 MiB RSS in Rust while FPM is the actual gate. On overflow the code returns **502**; the comment claims 413 (`lib.rs:281-289`). | MEDIUM | `lib.rs:282-289` | nginx `client_max_body_size` → 413; Apache `LimitRequestBody` → 413 |
| F5 | No deadline on the response body stream. The connect and header phases are wrapped in `timeout`; `CgiBodyStream` is not (`lib.rs:376-440`). The comment at `lib.rs:128-130` claims the transport enforces a body timeout — hyper imposes no such deadline on streamed bodies. A stalled PHP response holds the client and FPM connections open indefinitely. | MEDIUM | `lib.rs:376-440` | nginx `proxy_read_timeout` (applies between reads, i.e. an idle timeout) |
| F6 | Un-normalized URIs forwarded to PHP: `derive_script_info` splits on the first literal `.php` in the raw path (`lib.rs:537-544`); matchit does not collapse dot-segments. `GET /apps/files/../../x.php` flows `..` into `NC_ORIGINAL_SCRIPT`/`SCRIPT_NAME`/`SCRIPT_FILENAME` → the shim's `realpath()` fails → at minimum PHP TypeError/500, at worst a bogus `WEBROOT` in rendered URLs. (`try_static_files` already rejects `..`; the proxy does not.) | MEDIUM | `lib.rs:537-544` | nginx normalizes dot-segments during URI processing before location matching |
| F7 | Docs/comment claim client `X-NC-Proxied` is stripped (`deployment.md:220-222`, `lib.rs:266-268`); it is not in the strip list (`lib.rs:230-237`). Neutralized today by HashMap overwrite + post-loop injection — the trust boundary rests on implicit ordering. A refactor that moves injection above the header loop, or a crate upgrade to an ordered multi-map, silently yields **full authentication bypass**. | MEDIUM (latent) | `lib.rs:228-275` | n/a — our own contract |
| F8 | Everything outside the 5-entry strip list is forwarded: hop-by-hop headers (`Connection`, `Upgrade`, `Proxy-Authorization`, …); no cap on `header_accum` while scanning for the CGI separator (`lib.rs:470-495`). | LOW | `lib.rs:228-247, 470-495` | canonical `fastcgi_params` passes a curated set |

---

## Sequencing

```
Wave 0  Decision spikes (days, parallel)          ── shapes Wave 2
Wave 1  Production gate: F1, F2                   ── gates any production claim
Wave 2  Resource governance: F3, F4, F5           ── depends on Wave 1's client identity
Wave 3  Trust-boundary hygiene: F6, F7, F8        ── independent, cheap, anytime
Ongoing Differential parity corpus                ── seeded in Wave 1, grows per wave
```

Wave 0 exists so Wave 2 is built against the final session-resolution architecture, not a throwaway. Waves 1 and 3 are independent of each other; within Wave 1 the two items are independent and parallelizable.

---

## Wave 0 — Decision spikes

Goal: resolve the two questions whose answers change downstream design, and verify one load-bearing assumption in the shim. Deliverables are short written decisions appended to this file, not code.

### 0.1 Session resolution: direct store read vs. hardened round-trip

The `__session_resolve` round-trip is the largest coupling point in the system: a full PHP bootstrap on the hot path of every cookie-authenticated browser request, a `Set-Cookie` relay, a 60 s positive cache, and (per F3) uncached failures.

Verified context: the dev stack runs PHP sessions on the **default file handler** — no `session.save_handler` override in `docker/php84/` or `docker/configs/`, and no session keys in `data/additional.config.php`. Nextcloud's `config.php` memcache settings do not cover PHP sessions; moving sessions to Redis is a `php.ini`-level deployment change. The spike therefore targets **file-backed sessions first**:

- Prototype a reader for `sess_{id}` files (PHP's default serialization format is parseable; the fields we need are `__ACTIVE_USER` / the uid and `AUTHENTICATED_TO_DAV_BACKEND` — verify exact key names against `lib/private/Session/Internal.php` and the session writes in `OC\User\Session` during the spike, not here).
- Redis-backed reading is a stretch option only for deployments that opt into a Redis session handler; it must not become a deployment requirement.
- Measure: round-trip p50/p99 vs. direct read; revocation-lag improvement (TTL-bound 60 s → ~0).

Decision criteria:
- **Go** if the file-store reader is correct across PHP's session serialization edge cases (lazy write, `session_regenerate_id` rename races, locked-session semantics — PHP holds an exclusive lock on `sess_{id}` while a request runs; Rust reads must tolerate partial/locked files): implement the direct read; keep `__session_resolve` only for the remember-me (`nc_token`) path, which must run PHP's `loginWithCookie()` for token rotation side effects.
- **No-go** if the serialization/locking edge cases prove fragile: harden the round-trip instead (Wave 2.1, in full).

### 0.1 DECISION (2026-08-10, revised) — Feasible but NOT worth it: hardened round-trip selected

Experiment evidence:

- **Session store**: file-backed at `/tmp` (save_path unset → PHP default on this
  image), PHP default serialization wrapper, **content encrypted**:
  `encrypted_session_data|s:N:"<ciphertext-hex>|<iv-hex>|<hmac-hex>|3"` — the
  plaintext inside is `json_encode($sessionValues)` (`CryptoSessionData.php:190`).
  The plan's original assumption (plain `__ACTIVE_USER` readable in the file) is
  wrong on NC 30+ — the file only holds the sealed blob.
- **Passphrase**: `oc_sessionPassphrase` cookie, **plaintext**
  (`CryptoWrapper.php:47` reads it directly; 128 random chars when absent).
  Rust already reads the Cookie header → has it.
- **Decryption scheme** (`Crypto.php:encrypt`, phpseclib 2.x `Crypt\AES`):
  HKDF-SHA512(passphrase) → 32 B AES-key material + 32 B HMAC key; AES key =
  **phpseclib's PBKDF2-SHA1 quirk** (`setPassword($keyMaterial)` → pbkdf2 with
  password-as-salt, 1000 iterations, 16 B output — pin against the live stack,
  not from memory); AES-CBC; encrypt-then-MAC: HMAC-SHA512 over
  hex(cipher)+hex(iv); blob version `3`.

**First verdict (2026-08-10):** go, conditional — the port is implementable
(Rust has the cookie + config secret; hkdf/hmac/sha2/aes/cbc/pbkdf2 are
standard; lock semantics are acceptable — PHP's exclusive flock means the
reader sees the last *committed* state, partial writes fail the parse →
bounded retry).

**Revised verdict (2026-08-10): do not build the direct read.** Weighed
against the project goals, the cost exceeds the benefit:

1. It serves only cookie-authenticated **browser** requests — sync clients
   (app tokens) never touch the session resolver, and the 60 s positive cache
   already amortizes the PHP bootstrap to ≤1 per session per minute. The perf
   goal's primary audience is unaffected.
2. It pins an **undocumented, version-coupled PHP runtime format** (session
   serialization + phpseclib crypto). The parity corpus cannot diff it (it is
   not HTTP behavior), and crypto exactness is unforgiving — a wrong byte is a
   silent wrong identity, not a red test.
3. **Deployment constraint**: Rust must read PHP-FPM's session files — a
   shared, permission-accessible session path in any topology where the two
   are not co-located.

The two problems it would solve have cheaper fixes in the hardened path:
- F3 (exhaustion) → 2.1 negative caching + concurrency cap (pure Rust, no
  format coupling).
- Revocation lag → the positive cache TTL is a knob (60 s → 10-15 s).

**Consequence**: Wave 2.1 is the hardened round-trip in full (negative
caching + concurrency cap + optional per-IP rate limit); `__session_resolve`
stays the session path.

### 0.2 DECISION (2026-08-10) — FPM DOES populate `$_COOKIE`; the shim's manual parse is redundant

Experiment evidence: a hand-built FastCGI request (`HTTP_COOKIE: probe=1;
session=sess123`) to the dev FPM socket (bypassing the shim) executed a probe
script dumping `$_COOKIE` and `$_SERVER['HTTP_COOKIE']`:

```
array(2) { ["probe"]=> string(1) "1", ["session"]=> string(7) "sess123" }
string(24) "probe=1; session=sess123"
```

**Decision**: the shim's manual cookie parse (`php-shim/index.php:531-562`) is
redundant — FPM's `sapi_activate` already parses `HTTP_COOKIE` into `$_COOKIE`.
Keep the parse as defense-in-depth (it costs nothing and protects deployments
that don't forward `HTTP_COOKIE`), but **correct the comment**: the documented
claim ("FPM does not populate $_COOKIE") is false on PHP 8.4 FPM; record the
verification. Proxied routes see cookies natively — PHP-side session/CSRF
behavior on them is consistent with the canonical stack.

### 0.2 Verify PHP-FPM `$_COOKIE` population (runtime experiment)

This is the one item that cannot be settled from source: the shim asserts PHP-FPM does **not** populate `$_COOKIE` from the `HTTP_COOKIE` FastCGI param and parses cookies manually (`php-shim/index.php:531-562`). Standard FPM internals (`fpm_main.c` sets `SG(request_info).cookie_data` from `HTTP_COOKIE`; `sapi_activate` parses it into `$_COOKIE`) suggest otherwise — but the shim author presumably observed what they documented.

Experiment: one 10-line PHP script behind the dev FPM socket, invoked via a hand-built FastCGI request with `HTTP_COOKIE: probe=1`; dump `$_COOKIE` and `$_SERVER['HTTP_COOKIE']`.

Consequences:
- If FPM *does* populate `$_COOKIE`: the manual parse is redundant (harmless, leave it), and normal proxied requests see cookies natively — confirm PHP-side session/CSRF behavior on proxied routes relies on that, and document it.
- If it does not (environment-dependent quirk): the shim comment is load-bearing; document the FPM version/config under which it holds, because the normal proxy path then has a latent cookie-visibility gap on every proxied route.

---

## Wave 1 — Production gate

These two items gate any claim of production readiness. F1 is exploitable today with zero preconditions; F2 nullifies brute-force protection in the documented deployment.

### 1.1 Static file serving: deny list (F1) — DONE (superseded by the Phase 18.1 whitelist)

> **RESOLVED — 2026-08-10, superseded by the Phase 18.1 static whitelist.**
> `try_static_files` now serves **only** `/core/ /dist/ /themes/ /apps/` +
> `robots.txt` + `index.html` (the `STATIC_PREFIXES` whitelist in `router.rs`),
> rejecting everything else with 404 **before** the `metadata()` call. The
> whitelist is strictly stronger than the deny list below: it denies the F1
> set (`data config lib 3rdparty build tests templates console occ issue
> indie autotest* db_* dotfiles`) plus every other non-asset path, with the
> same no-existence-probing property. Live-verified 2026-08-10: `/data/.ocdata`
> `/config/CAN_INSTALL` `/3rdparty/*` `/lib/*` `/templates/*` `/.git/*` `/occ`
> all → 404; `/core/img/actions/add.svg` `/robots.txt` → 200.
>
> **Remaining work from 1.1 (the deny behavior exists but is unpinned):**
> 1. Unit tests around the whitelist (`static_denies_data_directory`,
>    `static_denies_dotfile_path`, `static_denies_3rdparty`,
>    `static_denied_prefix_case_insensitive`, `static_php_falls_through`).
> 2. Live probes in the parity gate (`curl /data/.ocdata` → 404).
> 3. `docs/deployment.md` operator guidance: the recommended nginx block must
>    carry the admin-manual deny rules — the web server is the canonical
>    layer for this (PHP relies on `.htaccess`/nginx), and Rust's whitelist is
>    defense-in-depth, not a substitute.

**Decision:** prefix deny list + dot-segment rejection, returning **404** for denied prefixes — matching the current Nextcloud nginx admin manual, which uses `return 404` on exactly these prefixes (404 also avoids leaking whether a denied path exists; this deliberately deviates from Apache's `Require all denied` → 403 semantics, and we document that). Keep `try_static_files` **outside** the maintenance guard — this is parity-correct: in the canonical stack maintenance mode is enforced inside PHP (`lib/base.php`), and the webserver keeps serving assets, which is what makes PHP's styled maintenance page work.

**Approach**, in `router.rs::try_static_files`:
1. Deny any path whose first segment (case-insensitive) is in: `data`, `config`, `lib`, `3rdparty`, `build`, `tests`, `templates`, `console`, `occ`, `issue`, `indie` — plus prefixes `autotest*` and `db_*` — plus any segment starting with `.`. This is the exact set from the two canonical nginx `location` regexes cited in F1; if those regexes change upstream, this list changes with them.
2. Keep the existing `.php` pass-through and `..` rejection.
3. 404 before the `metadata()` call (no existence probing of denied paths).

**Verification:**
- Unit (`cargo test -p nc-server`, new tests around the middleware with a tempdir root): `static_serves_existing_app_asset` → 200; `static_denies_data_directory` → 404 even when the file exists; `static_denies_config_directory` → 404; `static_denies_dotfile_path` (`/.git/HEAD`) → 404; `static_denies_3rdparty` → 404; `static_denies_occ` → 404; `static_php_falls_through_to_router` → passes to `next`; `static_denied_prefix_case_insensitive` (`/DATA/x`) → 404.
- Live: against a dev instance, `curl /data/.ocdata` → 404 (was 200), `curl /core/img/favicon.ico` → 200, `curl /config/CAN_INSTALL` → 404.

**Tradeoff accepted:** an extension allow-list (root `.htaccess` `FilesMatch` set: `css|js|mjs|svg|gif|png|jpg|webp|ico|wasm|tflite|woff2?|otf`) would be stricter but breaks any app shipping an unusual static type; the deny list mirrors what the canonical webserver configs actually enforce. Revisit if app auditing shows sensitive non-php files outside the denied prefixes.

### 1.2 Trusted proxies and client identity (F2)

**Decision:** port PHP's algorithm **line-for-line** — do not re-derive. One resolution per request, shared by auth, proxy, and logging.

**Approach:**
1. New module `nc-server/src/client_identity.rs` exposing `ClientIdentity { ip: IpAddr, proto: http|https, host: String, port: u16 }` and `resolve(headers, peer_addr, config) -> ClientIdentity`.
2. Port, citing as you go:
   - `getRemoteAddress()` (`Request.php:≈571-611`): only if `peer_addr` matches `trusted_proxies` (CIDR-capable — new dependency `ipnet`, mirroring Symfony `IpUtils::checkIp`), walk `forwarded_for_headers` (config, default `["X-Forwarded-For"]`) in reverse header order, entries right-to-left, strip `[v6]:port` and `v4:port` forms, skip entries that are themselves trusted proxies, return first `IpAddr`-parseable entry; fallback = peer address. Malformed config entries → log + no match (PHP: `error_log` + false).
   - `getServerProtocol()` (≈630-670): `overwriteprotocol` when `overwritecondaddr` regex matches peer (≈617-621), else `X-Forwarded-Proto` (first of comma list) **only from a trusted proxy**, else `HTTPS` param, else http; invalid value → http + `warn!` (PHP logs `critical`).
   - Host — port `getInsecureServerHost()` (`Request.php:799-821`) exactly: `overwritehost` or `X-Forwarded-Host` (first of comma list) **only from a trusted proxy**; else `Host` → `SERVER_NAME` → `localhost`.
   - `trusted_domains` enforcement for native routes — port `lib/base.php:872-912`, which PHP runs inside `OC::init` before any auth: when `installed` and the resolved host is untrusted — pathinfo `/css/*` passes through; `/status.php` gets 400 with exactly `{"error": "Trusted domain error.", "code": 15}`; everything else gets 400 + `info` log (`remoteAddress`, `host`) + error page, then exit. **Deviation (document):** PHP renders the themed `core/untrustedDomain` guest template; Rust returns a minimal error page with the same docs link — status, gating, and exemptions identical, body chrome differs (rendering a PHP template here would require an FPM round-trip for a 400 page). Separately, `getServerHost()` (`Request.php:828-845`) falls back to the *first* trusted domain — that is the value Rust must use wherever it generates absolute URLs, distinct from the enforcement check.
3. Config surface: `NcConfig` gains `trusted_proxies: Vec<String>`, `forwarded_for_headers: Vec<String>`, `overwritehost`, `overwriteprotocol`, `overwritecondaddr` (verified: the `nc-db/src/config.rs` parser already handles strings, bools, ints, and multi-line PHP arrays — `trusted_domains` at `config.rs:62`, fixture at `config.rs:441`; only new struct fields are needed).
4. Wiring: peer address requires `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())` at `main.rs:240` (currently plain `into_make_service` semantics — nothing uses `ConnectInfo` today). Resolve once in a middleware just inside the trace layer; store `ClientIdentity` as a request extension.
5. Consumers switch to the resolved values: `auth.rs` throttle key and `extract_client_ip` deletion; `proxy_handler` `REMOTE_ADDR`/`SERVER_NAME`/`SERVER_PORT`/`HTTPS` params; the `is_https` flag feeding `check_samesite_cookies`.
6. Forward the original `X-Forwarded-*` headers to PHP unchanged. Consistency argument (document in the module): Rust stops at the first untrusted entry from the right and hands PHP that IP as `REMOTE_ADDR`; PHP runs the identical algorithm under the identical config, sees an untrusted `REMOTE_ADDR`, and returns it directly — both layers converge. Chained-proxy configs stay coherent for the same reason.

**Verification:**
- Unit (`cargo test -p nc-server`, module `client_identity`) — one test per PHP behavior: `peer_addr_used_when_no_trusted_proxies_configured`; `peer_addr_used_when_peer_not_trusted`; `single_proxy_returns_client_ip`; `chained_proxies_skip_trusted_right_to_left`; `untrusted_entry_stops_the_walk` (spoofed leftmost entry ignored); `ipv4_with_port_stripped`; `ipv6_bracketed_with_port_stripped`; `malformed_entry_skipped`; `cidr_range_matches`; `malformed_config_entry_never_matches`; `forwarded_host_ignored_from_untrusted_peer`; `overwritehost_applies_only_from_trusted_proxy`; `proto_from_xff_only_when_trusted`; `proto_comma_list_takes_first`; `overwriteprotocol_gated_by_condaddr`; `invalid_proto_falls_back_to_http`. Plus enforcement (mirroring `base.php:872-912`): `untrusted_host_status_php_returns_json_400` (exact body `{"error": "Trusted domain error.", "code": 15}`); `untrusted_host_css_pathinfo_passes_through`; `untrusted_host_other_returns_400_page`; `untrusted_check_skipped_when_not_installed`; `url_generation_uses_first_trusted_domain_fallback`.
- Live: from a non-trusted address, `curl -H 'X-Forwarded-For: 1.2.3.4' /status.php` with failed Basic auth → `oc_bruteforce` row records the **real** peer IP, not 1.2.3.4; from a trusted proxy address the recorded IP is the rightmost untrusted XFF entry.

**Note for `docs/deployment.md`:** the recommended nginx block must grow `set_real_ip`-equivalent guidance — i.e., operators must configure `trusted_proxies` for their topology, exactly as they must for PHP.

---

## Wave 2 — Resource governance

Depends on Wave 1.2 only for per-IP rate limiting (2.1, optional); 2.2-2.4 are independent.

### 2.1 Session-resolution exhaustion (F3)

**DECIDED (Wave 0.1): no-go on the direct store read** — the hardened
round-trip in full:
1. **Negative caching:** cache `uid: null` results under the same SHA-256 cookie key with a short TTL — initial value 5 s, tunable; long enough to absorb an attacker's request burst, short enough that a just-completed PHP login is barely delayed. This alone kills the amplification: repeated junk-cookie requests hit memory, not FPM.
2. **Concurrency cap:** `tokio::sync::Semaphore` around `resolve_session` with permits ≈ `pm.max_children / 2` (configurable) — FPM-saturating traffic must not be able to come entirely from resolution. Excess waits with a bounded timeout, then falls through anonymous.
3. **Optional, needs Wave 1.2:** per-IP resolution rate limit reusing the brute-force machinery under a distinct action key (`session_resolve`). Skip if negative caching + semaphore prove sufficient under load — don't add throttle-key semantics speculatively.
4. Document the `Set-Cookie` rotation interaction: cached identities skip PHP's `loginWithCookie()` rotation, so remember-me tokens rotate at most once per TTL window, not per request. Deliberate, parity-adjacent, note it.

**Verification:** unit — `negative_result_cached_within_ttl`, `negative_cache_expires_after_ttl`, `positive_and_negative_entries_share_eviction`. Live/load — `N = 4 × pm.max_children` parallel `PROPFIND /remote.php/dav/files/x/` with junk session cookies + guard cookies: FPM worker pool stays above water (PHP-routed routes still answer), Rust RSS bounded, no 5xx storm on the native DAV path.

### 2.2 Body limits with correct status (F4)

1. On `to_bytes` overflow return **413** with `Retry-After`-free plain body (parity: nginx/Apache). Fix the comment/code mismatch at `lib.rs:281-289`.
2. Global in-flight cap: `Arc<Semaphore>` with byte-denominated permits is awkward; simpler and sufficient — a counting semaphore bounding *concurrent* buffered bodies (e.g. 64 × per-request cap worst case = a stated, configured RSS ceiling). Requests beyond the cap get 503 + `Retry-After: 1` rather than queueing unboundedly.
3. Longer term (not this wave): stream request bodies to FCGI stdin instead of buffering — `fastcgi-client` takes an `AsyncRead` for stdin, so the buffer is a choice, not a constraint. Defer until profiling shows the copy matters; the semaphore bounds the risk meanwhile.

**Verification:** unit — `body_over_limit_returns_413_not_502`. Live — 65 parallel 64 MiB POSTs to a PHP-routed route: RSS stays under the documented ceiling, excess requests get 503, PHP-routed GETs stay responsive.

### 2.3 Response-body deadline (F5)

Per-chunk **idle** timeout on `CgiBodyStream` (mirror of `proxy_read_timeout` semantics): reset a deadline on every yielded chunk; on expiry, end the stream and `warn!`. Default 60 s (nginx's default `proxy_read_timeout`), configurable alongside `fastcgi_timeout_ms`.

**Deviation (document):** mid-body failure on HTTP/1.1 cannot be reported as 504 — the status line is already sent. The stream is severed (client sees a truncated body / connection reset), which is exactly what nginx does on `proxy_read_timeout` mid-body. Delete the incorrect "enforced by the transport layer" comment at `lib.rs:128-130`.

**Verification:** unit — `stalled_body_stream_severed_after_idle_timeout` (fake `ResponseStream` that yields one chunk then pends forever; assert the adapter errors/ends after the configured idle). A whole-response wall-clock cap is explicitly **not** added: large legitimate responses (export zips) would be killed arbitrarily; idle detection is the correct knob.

### 2.4 CGI header accumulator cap (F8, partial)

Cap `header_accum` at 64 KiB while scanning for `\r\n\r\n`; overflow → 502 + `warn!`. Legitimate CGI header blocks are a few hundred bytes (the shim's own responses are under 1 KiB), so 64 KiB is generous headroom; anything beyond it is a misbehaving app and this is pure containment.

---

## Wave 3 — Trust-boundary hygiene

Cheap, independent, order irrelevant. None is exploitable today; all prevent *future* exploitability.

### 3.1 Make the marker guarantee explicit (F7)

1. Add `x-nc-proxied` to the strip list at `lib.rs:230-237`.
2. Extract param construction out of `proxy_handler` into a pure `build_fcgi_params(parts, auth_info, client_identity, fpm) -> Params` (also needed to make any of this testable — today it's untestable inline code).
3. Regression tests pinning the invariant that no client input can set trust params:
   - `client_x_nc_user_header_stripped` — request carrying `X-NC-User: evil` + no AuthInfo → no `HTTP_X_NC_USER` in params.
   - `client_x_nc_is_admin_stripped` — same for `X-NC-Is-Admin: 1`.
   - `client_x_nc_proxied_cannot_override_injection` — client sends `X-NC-Proxied: 0` → param is `"1"`.
   - `all_http_x_nc_params_are_proxy_injected` — iterate final params; every key starting `HTTP_X_NC_` has exactly the value Rust chose, for both authenticated and anonymous inputs.
4. Fix the stale comments at `lib.rs:266-268`. `deployment.md:220-222` is already correct in intent — code now matches it.

### 3.2 URI normalization at the proxy boundary (F6)

Reject (404) any proxied path containing a `..` segment or a case-insensitive `%2e` before `derive_script_info` — same rule `try_static_files` already applies.

**Deviation (document):** nginx silently normalizes dot-segments rather than rejecting; we 404. Legitimate clients never send dot-segments; rejection is simpler, loggable, and avoids inventing a normalization pass.

Optionally, allow-list entry scripts (`index.php`, `remote.php`, `public.php`, `ocs/v1.php`, `ocs/v2.php`, `status.php`, `ocs-provider/index.php`, `cron.php` if/when I.6 lands) mapping anything else `*.php` to the shim's default `OC::handleRequest()` — which is what happens today anyway, since the shim never includes the target. Low value; do it only if it falls out of the refactor for free.

**Verification:** unit — `dot_segment_path_rejected`, `encoded_dot_segment_rejected`, `normal_path_info_preserved`. Live — `GET /apps/files/../../index.php` → 404, PHP never invoked.

### 3.3 Hop-by-hop header hygiene (F8, remainder)

Extend the strip list with hop-by-hop headers that have no meaning to FPM: `connection`, `keep-alive`, `upgrade`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`. Keep `authorization` forwarding — canonical `fastcgi_params` passes it and PHP's own login chain consumes it on unauthenticated proxied routes; note in the module docs that the `raw_token` curation in `auth.rs:286-292` is defense-in-depth, not the confidentiality boundary for passwords.

---

## Ongoing — Differential security-parity corpus

The long-term risk is drift between the Rust and PHP auth/edge stacks. The defense is mechanical, not disciplinary: a request corpus executed against **both** the PHP reference stack and `nc-server`, diffing status + normalized headers + body.

- Home: `build/security-parity/` next to the existing integration harness — verified: `workspace/server/build/integration/` exists with per-area Behat feature directories (`dav_features`, `capabilities_features`, …); the corpus runner reuses that harness's server bootstrap and diffs responses from both stacks.
- Seed corpus = every attack case from this assessment: XFF spoofing matrices (trusted/untrusted peer × single/chained/malformed XFF), cookie-guard combinations, static-deny paths, `X-NC-*` injection attempts, dot-segment paths, oversize bodies, junk-session-cookie floods.
- Every subsequent wave adds its cases to the corpus as it lands; a diff is a bug in one implementation or the other, triaged like a parity issue.
- Run it in CI against the dev compose's PHP stack; known intentional deviations (documented above: 404-vs-normalize, 403 semantics details, rotation cadence) are listed in an exceptions file, never silently.

---

## Explicitly out of scope

| Item | Why not |
|---|---|
| Inverting the topology (PHP edge, Rust backend) | Forfeits the rewrite's purpose — Rust must hold sync-client connections to solve FPM starvation. Considered and rejected in the reassessment. |
| Replacing the shim / marker scheme | Audited sound; the socket + marker + strip model is the minimal correct shape for this split. |
| Connection pooling for FPM | Deliberately short-connection (`lib.rs:8-31`), revisited only if §8 load testing proves connect cost. Not a security item. |
| Native-route `isEnabled()` re-check on cached session identities | Separate question from the forwarding path; verify within Phase 4/9 native-handler work, not here. Flagged for follow-up. |
| CSRF enforcement on proxied state-changing requests | PHP's AppFramework enforces it inside `OC::handleRequest()`; nothing to replicate Rust-side. Wave 0.2's `$_COOKIE` verification confirms the premise. |

## Relationships to deferred improvements

- **I.1** (config hot-reload): `trusted_proxies`/`overwrite*` inherit I.1's restart-to-apply behavior until I.1 lands. Acceptable — matches how PHP operators already treat most system config.
- **I.4** (token revocation window, 5 min): orthogonal; the session cache here is 60 s and independent. If Wave 0.1 goes Redis, revisit I.4's eviction approach at the same time.
- **I.6** (`/cron.php` not routed): add to the 3.2 allow-list when I.6 lands.

## Exit criteria

Phase done when:
1. F1 live checks pass: `/data/**`, `/config/**`, dotfiles → 404; app/static assets → 200.
2. F2 live checks pass: brute-force rows and PHP `REMOTE_ADDR` reflect Wave 1.2 resolution under both trusted and untrusted topologies; all 16 `client_identity` unit tests green.
3. Junk-cookie flood load test (2.1) holds FPM responsiveness at `4 × pm.max_children` adversarial concurrency.
4. Oversize POST → 413; stalled backend stream severed at idle timeout; RSS ceiling holds under 2.2 load test.
5. `all_http_x_nc_params_are_proxy_injected` and the 3.2 traversal tests green; `HTTP_X_NC_*` cannot be client-set under any crafted header set.
6. Parity corpus seeded and green in CI, exceptions file empty of unjustified entries.
7. Wave 0 decisions written into this file; `docs/deployment.md` updated for `trusted_proxies` operator guidance.
