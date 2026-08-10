# Phase 15 — Edge security hardening

Task tracking for [`SPECS/03-implementation-plan/plan/15-edge-security-hardening.md`](../03-implementation-plan/plan/15-edge-security-hardening.md) — the security reassessment of the PHP-FPM forwarding path. Waves 1-3 + the parity corpus; status via checkboxes only.

## Wave 0 — Decision spikes

- [x] **0.1 Session resolution: direct store read vs. hardened round-trip — DECIDED 2026-08-10 (feasible, NOT worth it — hardened round-trip).** Evidence: session files are encrypted (`encrypted_session_data|s:N:"cipher|iv|hmac|3"`, plaintext = JSON), the `oc_sessionPassphrase` cookie is the plaintext passphrase, decryption needs a phpseclib-exact port. Decided against: it serves only browser requests (sync = app tokens unaffected), pins an undocumented PHP runtime format (crypto exactness = silent wrong identity), and requires a shared session path in split topologies. F3 → negative caching + concurrency cap; revocation → cache-TTL knob.
- [x] **0.2 Verify PHP-FPM `$_COOKIE` population — DECIDED 2026-08-10.** FPM DOES populate `$_COOKIE` (probe dumped `["probe"=>"1","session"=>"sess123"]`); the shim's manual parse is redundant — keep as defense-in-depth, correct the comment.

## Wave 1 — Production gate

- [x] **1.1 Static file serving (F1) — DONE 2026-08-10.** Superseded by the Phase 18.1 `try_static_files` whitelist (`/core /dist /themes /apps` + `robots.txt` + `index.html`), which is strictly stronger than the proposed deny list and 404s before the fs stat. Live-verified: all F1 paths → 404. Resolution note in the plan.
- [ ] **1.1 remainder: pin the security property.** Whitelist deny-path unit tests (`static_denies_data_directory`, `static_denies_dotfile_path`, `static_denies_3rdparty`, `static_denied_prefix_case_insensitive`), a live probe in the parity gate (`curl /data/.ocdata` → 404), and the `docs/deployment.md` nginx deny-rules operator guidance (the canonical layer's version of the same rule).
- [x] **1.2 Trusted proxies and client identity (F2) — DONE 2026-08-10.** Port PHP's `getRemoteAddress`/`getServerProtocol`/`getInsecureServerHost` line-for-line (`client_identity.rs` — manual v4/v6 CIDR masking, no `ipnet` dep); `trusted_domains` enforcement for native routes mirroring `lib/base.php:872-912`; `ConnectInfo` wiring; consumers (throttle key, proxy `REMOTE_ADDR`, SameSite `is_https`) switch to the resolved identity; 24 unit tests + live A/B verification (spoofed XFF ignored, trusted-proxy XFF honored, untrusted Host → exact 400 body) + diff-test 20/20.

## Wave 2 — Resource governance

- [ ] **2.1 Session-resolution exhaustion (F3).** Hardened round-trip in full: negative caching (5 s TTL) of failed `__session_resolve` results, concurrency semaphore (`pm.max_children / 2`), optional per-IP rate limit; keep the positive session cache (revocation knob: TTL 60 s → 10-15 s).
- [ ] **2.2 Body limits with correct status (F4).** 413 (not 502) on `to_bytes` overflow; global in-flight body semaphore (documented RSS ceiling, 503 + `Retry-After: 1` beyond it).
- [ ] **2.3 Response-body deadline (F5).** Per-chunk idle timeout on `CgiBodyStream` (60 s default, `fastcgi_timeout_ms`-adjacent config); delete the wrong "transport enforces it" comment.
- [ ] **2.4 CGI header accumulator cap (F8, partial).** 64 KiB cap while scanning for `\r\n\r\n`; overflow → 502 + `warn!`.

## Wave 3 — Trust-boundary hygiene

- [ ] **3.1 Marker guarantee explicit (F7).** `x-nc-proxied` in the strip list; pure `build_fcgi_params`; regression tests pinning that no client input can set `HTTP_X_NC_*`.
- [ ] **3.2 URI normalization at the proxy boundary (F6).** Reject (404) `..` / `%2e` segments before `derive_script_info` (deviation: reject, don't normalize).
- [ ] **3.3 Hop-by-hop header hygiene (F8, remainder).** Strip `connection keep-alive upgrade proxy-* te trailer transfer-encoding`; keep `authorization` forwarding.

## Ongoing

- [ ] **Differential security-parity corpus.** Seed with the attack cases from this assessment (XFF matrices, cookie guards, static-deny paths, `X-NC-*` injection, dot-segments, oversize bodies, junk-session floods); diff Rust vs PHP responses; exceptions file for documented deviations; run in CI.

## Changes

- **2026-08-10 — 0.1 revised: feasible, not worth it.** The spike proved the direct session read is implementable (encrypted store, plaintext passphrase cookie, phpseclib port needed) but the goal analysis landed on the hardened round-trip: browser-only benefit, PHP-internal format coupling, deployment constraint. Wave 2.1 = negative caching + concurrency cap + TTL knob.
- **2026-08-10 — 1.1 closed by the Phase 18.1 whitelist; task doc created.** The static whitelist (a Phase 18 *performance* change) mechanically satisfies F1: only `/core /dist /themes /apps` + `robots.txt` + `index.html` are served, 404 before the fs stat, strictly stronger than the proposed deny list. The security property is currently an unasserted side effect — the 1.1-remainder item promotes it to tested invariants. Task doc records waves 0-3 + corpus as checkboxes.
