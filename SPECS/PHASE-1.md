# Phase 1 — HTTP Skeleton: Routing and Maintenance Mode

Goal: the binary listens on a port, routes every Nextcloud URL prefix to a placeholder handler or PHP-FPM stub, and correctly implements maintenance mode and `/status.php`.

---

### 1.1 HTTP listener
- [x] `axum` listener on configurable host:port (default `0.0.0.0:7000`)
- [x] Graceful shutdown on `SIGTERM`: drain in-flight requests within 30 s, then exit
- [x] `X-Request-Id` UUID generated per request, propagated through all handlers and log lines
- [x] Structured tracing: every request logs method, path, status, authenticated user (or `-`), response time, request ID

**Verify:** `curl -i http://localhost:7000/status.php` returns a response (any body); log output contains method, path, status, and a UUID. ✅

### 1.2 `/status.php`
- [x] Returns `Content-Type: application/json`, `Access-Control-Allow-Origin: *`
- [x] JSON body with all fields from REQ §3: `installed`, `maintenance`, `needsDbUpgrade`, `version`, `versionstring`, `edition`, `productname`, `extendedSupport`
- [x] `installed` and `maintenance` read from `NcConfig` (parsed from `config.php` — PHP writes these via `SystemConfig`, not `oc_appconfig`). Version strings and all other fields read from `AppConfigCache` (`oc_appconfig`) — no direct DB query per request.
- [x] `/status.php` is always served, even when `maintenance = true`

**Verify:** `build/integration/features/maintenance-mode.feature` — the maintenance-mode status assertion passes; JSON fields present and correctly typed under `jq`. ✅

### 1.3 Maintenance mode middleware
- [x] Middleware runs before all handlers except `/status.php` and `/heartbeat`
- [x] When `maintenance = true`: respond `503`, headers `X-Nextcloud-Maintenance-Mode: 1`, `Retry-After: 120`, body as plain text for DAV or OCS envelope for OCS routes
- [x] Reads `NcConfig.maintenance` (from `config.php` at startup) — no DB query per request. Toggling maintenance requires a server restart (PHP writes to `config.php`, not `oc_appconfig`).

**Verify:** `build/integration/features/maintenance-mode.feature` — all 503 scenarios pass. Toggle `maintenance` in DB, confirm next non-status request returns 503 without server restart. ✅

### 1.4 Route table
- [x] Register all URL prefixes from REQ §2.1 as `axum` routes
- [x] Native Rust routes: `/status.php`, `/heartbeat`, `/ocs/v1.php/…`, `/ocs/v2.php/…`, `/ocs-provider/index.php` (JSON list of available OCS providers — REQ §2.1), `/remote.php/…`, `/public.php/…`, `/dav/…`, `/apps/files/api/…`
- [x] PHP-FPM stub routes (return `501 Not Implemented` until Phase 7): `/index.php`, `/.well-known/…` (forward with `X-NEXTCLOUD-WELL-KNOWN: 1` header per API_COMPATIBILITY.md §Well-known), `/login/…`, `/apps/…` (non-files)
- [x] Unknown routes return `404`

**Verify:** `build/integration/routing_features/apps-and-routes.feature` — correct status codes for each route prefix. ✅

### 1.5 `/heartbeat`
- [x] `GET /heartbeat` → `200 OK`, empty body
- [x] No auth required

**Verify:** `curl -i http://localhost:7000/heartbeat` returns `200`. ✅
