# Phase 12 handover — 2026-07-30 (end of session)

Status: **the real cause of "iOS app shows no files / never fires Depth:1" is identified — the `/cloud/capabilities` response is broken.** The earlier `oc:permissions` investigation (the original handoff) turned out to be chasing a stale capture and is **not** the cause. Details, evidence, and next steps below.

---

## 1. The goal

Make the Nextcloud iOS client (34.0.0) list files against the Rust server (`cloud2.home.lan`). Symptom: the app authenticates, does a root `PROPFIND`, and **never escalates to the Depth:1 listing**, so the files view is empty. Against PHP (`cloud.home.lan`) the same account works.

## 2. Headline finding

The iOS app fetches **`/ocs/v1.php/cloud/capabilities` before the listing PROPFINDs** (visible in the mitmproxy flow order) and uses it to decide the server supports the files feature. Rust's capabilities response is broken in two independent ways (§4), so the app treats the server as incompatible and never lists. **This is independent of the `oc:permissions` value** — which is why matching PHP's permissions (`RGDNVCK`) did not help.

## 3. What was investigated and ruled out (do not redo)

- **`oc:permissions` value — NOT the cause.** The user confirmed: when Rust returned `RGDNVCK` (matching PHP), the client *still* didn't fire Depth:1. The whole `LazyUserFolder → 15` mechanism investigation (see `phase-12-verification.md`) was based on a **stale** comparison.md PHP capture that showed `GDNVCK`/15. **Live PHP returns `RGDNVCK`/31 at both Depth:0 and Depth:1** (verified with 6 consecutive requests). See §6 — the spec docs and some tests now contain the wrong conclusion and need correcting.
- **Rust's Depth:1 endpoint — works.** `PROPFIND /files/{uid}/` with `Depth: 1` → `HTTP 207`, valid multistatus, root + child (`Photos`), correct `dav`/`vary`/`content-type` headers. The server *can* list; the client just doesn't ask.
- **Namespace / serialization cosmetics — not the cause.** Rust emits `<D:collection></D:collection>` vs PHP `<d:collection/>`, redundant `xmlns:d`, default-namespace elements in the 404 propstat, `"` vs `&quot;` in getetag. All standards-valid; any conformant parser reads them identically.
- **`nc:rich-workspace` / `nc:lock` 200-vs-404 — `[ENV]`, not the cause.** PHP puts them in the 200 propstat (Text/files_lock apps enabled on the capture server); Rust 404s them (absent on master). Spec decision stands: 404 is correct for the target env.
- **"Proxy capabilities to PHP" recommendation — RETRACTED.** Conflicts with the project goals (`SPECS/01-requirements/`): capabilities is **in-scope native Rust**, served from an in-memory cached payload (§5.6; problem statement headline win). The fix is to make the native path correct, not to proxy.

## 4. The two bugs (both in the native capabilities path)

Live Rust capabilities (`GET /ocs/v1.php/cloud/capabilities`, authenticated): **1649 bytes**, `version.major = 0`, keys = `[app_api, bruteforce, core, dav, files, theming]`. No `files_sharing`.
Live PHP capabilities: **6001 bytes**, full set (`files_sharing`, `provisioning_api`, `circles`, `notifications`, `ocm`, `user_status`, …), version `34.0.1`.

### Bug A — `version: 0.0.0` (clear, self-contained)
`core-rs/crates/nc-ocs/src/capabilities.rs:111-114`:
```rust
let version_str = ac.get_string("core", "oc_version")
    .or_else(|| ac.get_string("core", "version"))
    .unwrap_or_else(|| "0.0.0.0".to_string());
```
`oc_appconfig` has **no** `core/oc_version` or `core/version` row (verified) → falls back to `"0.0.0.0"` → `major: 0`.
**Fix:** read the version from the same source `status.php` uses — Rust's `status.php` handler correctly yields `34.0.1.2` (`GET /status.php` on cloud2 returns `"version":"34.0.1.2"`), so find that source and reuse it. (`SPECS/.../03-status-php.md` claims version comes from `oc_appconfig` under `core`, but on this install it does not — status.php reads it from elsewhere; trace the status handler.)
**Secondary nit:** `status.php` sends `versionString`/`productName` (camelCase) vs PHP's `versionstring`/`productname` (lowercase). The `version` field itself matches. Align for strict clients.

### Bug B — the Phase 7.7 PHP-app capabilities fetch returns an incomplete set
The architecture is correct and per §5.6: fetch PHP-app caps from PHP-FPM at startup, cache, merge; 30s refresh. Wiring verified sound:
- `core-rs/crates/nc-server/src/main.rs:97-160` — startup fetch (admin + public).
- `main.rs:264+` `spawn_capability_refresh_task` — 30s refresh; calls `rebuild_capability_cache` then re-fetches.
- `core-rs/crates/nc-fastcgi/src/lib.rs:860+` `fetch_php_capabilities` (and `fetch_php_public_capabilities` ~line 997) — synthetic FastCGI OCS request to the shim.
- `core-rs/crates/nc-ocs/src/capabilities.rs:50-103` — `apply_php_capabilities` / `rebuild_serialized` / `merge_caps` (merge logic is correct and unit-tested).
- `core-rs/crates/nc-ocs/src/handlers.rs` `rebuild_capability_cache` — correctly snapshots + re-merges existing PHP caps (does not wipe).

Admin row exists (`SELECT uid FROM oc_group_user WHERE gid='admin'` → `admin`), so the fetch is attempted, not skipped.

**The problem:** the fetch only ever yields ~3 blocks (`app_api`, `bruteforce`, `theming`). Of the live response keys, `core/dav/files` are native (`build_capability_cache` builds *only* those three), so `app_api/bruteforce/theming` came from the PHP merge — i.e. the merge "succeeded" but with a **partial** set. `files_sharing`, `provisioning_api`, etc. never came through.

Evidence from `docker logs master-nextcloud-1`:
```
2026-07-29T12:17:36  INFO  Fetching PHP-app capabilities from PHP-FPM uid=admin
2026-07-29T12:17:36  INFO  PHP-app capabilities merged into capability cache
2026-07-29T12:17:36  INFO  PHP-app public (IPublicCapability) capabilities merged
2026-07-29T12:17:36  INFO  HTTP listener ready listen=0.0.0.0:80
# then, every 30s from 19:23:41 onward:
WARN  capabilities-fetch: PHP response missing ocs.data.capabilities
WARN  capability-refresh: PHP-app capabilities fetch failed; retaining existing cached PHP caps
```
Refreshes *succeeded* (debug-level) 12:17→19:23, then started failing every 30s. The current cache = native + `{app_api, bruteforce, theming}` (the startup merge value). The refresh failure is secondary — **even the successful startup fetch was partial.**

**Why partial (the open question — §5):** the shim (`core-rs/php-shim/index.php`) logs in as admin (`OCS DEBUG: ... loggedIn=1 userId=admin`) and calls `$appManager->loadApps()` (line ~193), so it is *not* the public-only path. Yet only ~3 capability providers come back. Hypothesis: most apps' capability providers are not **registered** in the shim's PHP execution context despite `loadApps()`, so `CapabilitiesManager` has few to return. (`workspace/server/lib/private/CapabilitiesManager.php::getCapabilities($public)` iterates registered `$this->capabilities` closures; includes a cap if `!$public || $c instanceof IPublicCapability`.)

## 5. The one remaining unknown + how to crack it

**Unknown:** why the shim's PHP context returns only ~3 capability blocks despite `loggedIn=1` + `loadApps()`.

**Decisive step:** capture the actual PHP response body of the synthetic fetch. `fetch_php_capabilities` only logs `body_preview` on a JSON-*parse* failure, not on the partial / "missing ocs.data.capabilities" case. Add a temporary `tracing::debug!(body = …)` for the raw body (or log it whenever `caps.is_none()`), rebuild, restart, and read `docker logs master-nextcloud-1`. That shows exactly what PHP returns and confirms whether it's an app-registration/load-order issue in the shim. Then fix so the fetch returns the full authenticated set (likely a shim bootstrap/app-loading fix, or how the OCS dispatch registers capabilities).

## 6. Corrections needed tomorrow (important)

The stale-capture detour left incorrect artifacts that must be reverted/corrected:

1. **The `& !16` home-root permission strip is WRONG.** `filesystem.rs::get_props` strips SHARE on `is_mount_root`, producing `GDNVCK`/15. Live PHP is `RGDNVCK`/31 at both depths. **The user will revert this themselves.**
2. **`phase-12.md` and `phase-12-verification.md` contain the wrong "canonical" conclusion** (the `LazyUserFolder → 15` mechanism, added 2026-07-30). The `LazyUserFolder` hardcode is real in source but only fires when `getUserFolder()` is called before `setupForUser` completes (a cold/first-request artifact); the normal/live behavior is `31`. These docs need a correction noting the comparison.md PHP capture was a stale/anomalous state (the handoff's own option (D)).
3. **11 regression tests assert the wrong behavior** (added 2026-07-30): `row::tests::home_root_permission_pipeline_matches_php_capture` (asserts effective=15), `row::tests::apply_sharing_mask_*`, `row::tests::compute_share_permissions_*`, `row::tests::permissions_to_ocm_json_*`, and `props::tests::permissions_dir_home_root_share_stripped` (asserts `15 → GDNVCK`). The pure-function encoding tests (`encode_permissions(15)→GDNVCK`, ocm json, etc.) are still valid as encoding facts; the *home-root = 15* framing/assertions are wrong and should be corrected or removed when the `& !16` strip is reverted. Test count was 284; will drop when these are adjusted.
4. The **12.14–12.16 "deferred findings"** appended to `phase-12.md` (can_rename parent-CREATE gap, home-root hardcode-vs-derive, share/mount paths) are still valid observations — but 12.15's premise (home root should be 15) is now inverted: live PHP home root is 31, so the open question there is moot/different.

## 7. Environment facts (for quick restart tomorrow)

- **Servers:** `cloud.home.lan` = PHP reference, `cloud2.home.lan` = Rust. Both resolve to `10.99.0.2` and route through `master-proxy-1` (nginx vhosts). Reachable from this host.
- **Docker is podman** ("Emulate Docker CLI using podman"). The Rust `nc-server` runs **inside** `master-nextcloud-1` (with PHP-FPM in the same container) — NOT a host systemd service. Logs: `docker logs master-nextcloud-1`.
- **Containers:** `master-proxy-1`, `master-nextcloud-1`, `master-database-pgsql-1` (postgres), `master-redis-1`, `master-mail-1`.
- **DB:** `docker exec master-database-pgsql-1 psql -U postgres -d nextcloud`. Same DB/instance (`ocecf7uk5jlr`, root fileid 79558) for both servers.
- **Auth tokens:** the iOS Basic-auth tokens for both servers are in `comparison.md` (Rust `RUST_AUTH`, PHP `PHP_AUTH`). comparison.md captures are **pre-fix** and the PHP one is the stale `GDNVCK` state.
- **Shim:** `core-rs/php-shim/index.php` (635 lines). Auth via `setVolatileActiveUser()` when `HTTP_X_NC_PROXIED=1` + `HTTP_X_NC_USER` set; OCS routed via `route_ocs_php()` → `Router::match('/ocsapp'.$pathInfo)` with `loadApps()` first.
- **PHP reference source:** `workspace/server/` (commit `e2dc439c715`).

## 8. Useful reproduction commands

```bash
UID=6c21875f5c096195a380c345979d02419c98359d28fad44432c4f579f26bc452
RUST_AUTH="Basic NmMyMTg3NWY1YzA5NjE5NWEzODBjMzQ1OTc5ZDAyNDE5Yzk4MzU5ZDI4ZmFkNDQ0MzJjNGY1NzlmMjZiYzQ1MjplUUxRci04Q2Zacy1vU1AyeS1aakhGWS1SUHNhTA=="
PHP_AUTH="Basic NmMyMTg3NWY1YzA5NjE5NWEzODBjMzQ1OTc5ZDAyNDE5Yzk4MzU5ZDI4ZmFkNDQ0MzJjNGY1NzlmMjZiYzQ1MjowYVNKTWdOQjhteFJjclRpa3p2QW5ybkZjbk5IZFo2VEJMNHh4MGFQQ3cyTDBya0hITTJSdWRZd0U4U3BlYTloQWE3cVM3WkY="

# Capabilities diff (the gate):
curl -sk "https://cloud2.home.lan/ocs/v1.php/cloud/capabilities" -H "Authorization: $RUST_AUTH" -H "OCS-APIRequest: true" -H "Accept: application/json" | python3 -m json.tool
curl -sk "https://cloud.home.lan/ocs/v1.php/cloud/capabilities"  -H "Authorization: $PHP_AUTH"  -H "OCS-APIRequest: true" -H "Accept: application/json" | python3 -m json.tool

# Root permission, Depth 0 vs 1 (PHP = RGDNVCK both; Rust currently GDNVCK both due to the bad strip):
curl -sk -X PROPFIND "https://cloud.home.lan/remote.php/dav/files/$UID" -H "Authorization: $PHP_AUTH" -H "Depth: 0" -H "Content-Type: application/xml" --data @/tmp/pf.xml | grep -o '<oc:permissions>[^<]*</oc:permissions>' | head -1

# Rust Depth:1 listing proof (works):
curl -sk -X PROPFIND "https://cloud2.home.lan/remote.php/dav/files/$UID/" -H "Authorization: $RUST_AUTH" -H "Depth: 1" -H "Content-Type: application/xml" --data @/tmp/pf.xml -w "\nHTTP=%{http_code}\n"

# Phase 7.7 fetch outcome:
docker logs master-nextcloud-1 2>&1 | grep -iE "capabilit|native-only|merged|Fetching PHP" | grep -v "OCS DEBUG" | tail -40
```
`/tmp/pf.xml` held the iOS PROPFIND body (the full `<d:prop>` list from comparison.md); recreate from comparison.md line 21 if gone.

## 9. Suggested order for tomorrow

1. Fix Bug A (version source) — small, independent, verifiable.
2. Crack Bug B: add the temporary body-capture log to `fetch_php_capabilities`, rebuild, restart, read the PHP response, fix the shim/registration so the fetch returns the full authenticated set. Verify live capabilities then includes `files_sharing` and `version.major=34`.
3. Have the user re-test the iOS app — if files appear, this was the gate.
4. Do the §6 corrections (revert `& !16` + fix tests + correct the two spec docs).
5. Only then, return to the genuine remaining PROPFIND parity items (the share-node permission matrix, `can_rename` parent-CREATE gap) which are real but not the stall cause.
