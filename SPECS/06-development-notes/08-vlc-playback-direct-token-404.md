# 08 — The Rust edge had no `/remote.php/direct` route, so the iOS client's one-time token URLs 404'd and VLC playback stopped before reaching `playing`

**Status:** fixed (commit `d40512a`). **Related:** [Phase 7.6 direct download URLs](../04-tasks/phase-7.md#76-direct-download-url) (the `{oc:}downloadURL` / `oc_directlink` surface), the DAV classification in the [router](../03-implementation-plan/plan/07-implement-dav-files-tree-properties.md).

## Observable failure

Playing a video from the Nextcloud iOS app (34.1.4) via the external VLC player failed: VLC accepted the presentation, buffered twice, then **stopped before reaching `playing`** ([log.txt](../../log.txt)). The app's own web client played the same file fine, and the video file / Range serving were healthy — which ruled out the file, MIME type, and byte-range handling.

## Root cause(s) — grounded

- **The iOS client does not hand VLC the plain DAV URL.** It mints a one-time token (`POST /ocs/v2.php/apps/dav/api/v1/direct` → `DirectController::getUrl`, `apps/dav/lib/Controller/DirectController.php`) and streams that token URL instead: `GET /remote.php/direct/{token}`.
- **Rust never routed `/remote.php/direct`.** `router.rs:build()` registered only `/remote.php/webdav` → native `dav_handler` and `/remote.php/dav` → `dav_arbiter_handler`; `php_fpm_fallback` covered `/public.php`, `/.well-known`, `/login`, `/index.php`, `/`, and the registry-built `/apps/*`, `/s`, `/f`, `/settings` entries. Nothing matched `/remote.php/direct`, so axum answered a bare 404 (empty body, no `dav:`/XML fingerprint) — the request never reached PHP's `dav/appinfo/v2/direct.php`.
- **The route registry can't discover it.** `build_route_registry` only emits entries for app-level paths and for `'root' => ''` routes parsed from `routes.php`; `direct#getUrl` lives under the `'ocs' => [...]` block (`apps/dav/appinfo/routes.php`), so no root-level entry is generated for it.
- **The php-shim already knew how to serve it** (`php-shim/index.php:353` maps `'direct' => 'dav/appinfo/v2/direct.php'`) — the only gap was the router line, not the PHP side.

## Options weighed

- **A. Register `/remote.php/direct` (and `/{*path}`) as `php_fpm_fallback`.** Chosen. The token path is cold and side-effect-bearing (writes/reads `oc_directlink`, fires `BeforeDirectFileDownloadEvent` — enforced by `files_sharing`'s `BeforeDirectFileDownloadListener` — and `BeforeFileDirectDownloadedEvent`, plus storage-specific signed URLs for object stores). Delegating preserves PHP parity with zero re-implementation. The hot `/remote.php/dav` route stays native and wins on specificity.
- **B. Implement `/direct` natively in Rust.** Rejected: would require replicating `oc_directlink`, token expiry, brute-force throttling, and both events for a cold path with no hot-path benefit. Not worth the surface.
- **C. A generic `/remote.php/{*path}` PHP fallback.** Considered as a superset (also covers caldav/carddav/calendar/contacts/files services). Kept minimal here — only `/remote.php/direct` — to avoid changing routing behavior for the other remote.php services without their own failure evidence; that broader catch-all remains a follow-up if those services surface.

## The choice

Add two routes in `core-rs/crates/nc-server/src/router.rs`:

```rust
.route("/remote.php/direct", axum::routing::any(php_fpm_fallback))
.route("/remote.php/direct/{*path}", axum::routing::any(php_fpm_fallback))
```

with a regression guard (`dav_served_by_rust_direct_is_not_native`) asserting Rust never claims the token path natively.

## Verification

- `cargo test -p nc-server --bin nc-server -- router::` — 12/12 pass (new test included).
- `cargo build -p nc-server` clean. Workspace suite: only the pre-existing `nc-preview` backend failures (`Operation not permitted` in the sandbox), unrelated to this change.
- Live A/B pending: after rebuild/redeploy, re-run the iOS VLC flow and the `GET /remote.php/direct/<token>` probe — expected a PHP `2xx` (file stream) instead of the Rust-edge 404.

## Follow-ups

1. Rebuild/redeploy (`make sut-image`, proxy restart) and re-verify the iOS VLC playback + the live token probe.
2. If other `remote.php` services (caldav/carddav/calendar/contacts/files) surface 404s through Rust, promote the explicit `/remote.php/direct` pair to a `/remote.php/{*path}` PHP-FPM fallback (the php-shim's service map already handles all of them).

Back: [`../README.md`](../README.md)
