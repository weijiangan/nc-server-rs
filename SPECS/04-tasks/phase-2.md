# Phase 2 — OCS Envelope and Core Endpoints

Goal: OCS v1 and v2 envelopes work correctly, format negotiation works, and the three core OCS endpoints return the right responses.

---

### 2.1 OCS envelope — XML
- [x] Default format is XML: `Content-Type: text/xml; charset=UTF-8`
- [x] XML structure matches REQ §5.1 exactly: `<ocs><meta>…</meta><data>…</data></ocs>`
- [x] v1: `<totalitems></totalitems>` and `<itemsperpage></itemsperpage>` present as empty strings
- [x] v2: `totalitems` and `itemsperpage` keys **omitted** from meta block

**Verify:** `build/integration/features/ocs-v1.feature` — envelope field assertions pass. ✅ (smoke tested)

### 2.2 OCS envelope — JSON
- [x] `?format=json` or `Accept: application/json` → `Content-Type: application/json; charset=utf-8`
- [x] JSON structure matches REQ §5.1
- [x] `?format=` query param takes precedence over `Accept` header

**Verify:** `curl -H "Accept: application/json" /ocs/v2.php/cloud/capabilities` returns valid JSON envelope; `jq .ocs.meta.status` = `"ok"`. ✅

### 2.3 OCS v1 HTTP status mapping
- [x] Any success → HTTP `200`
- [x] OCS statuscode `997` → HTTP `401`
- [x] Maintenance mode → HTTP `503`

**Verify:** `build/integration/features/ocs-v1.feature` — HTTP status code assertions. ✅ (unit tested)

### 2.4 OCS v2 HTTP status mapping
- [x] OCS `200`–`299` → same HTTP status
- [x] OCS `997` → HTTP `401`; `998` → `404`; `999` or unknown → `500`; outside `200`–`600` → `400`

**Verify:** integration test: craft OCS response with statuscode `998`, confirm HTTP `404`. ✅ (unit tested)

### 2.5 OCS unauthorised headers
- [x] XHR request (`X-Requested-With: XMLHttpRequest`) → `WWW-Authenticate: DummyBasic realm="Authorisation Required"` (OCS) / `DummyBasic realm="Nextcloud"` (DAV)
- [x] Non-XHR → `WWW-Authenticate: Basic realm="Authorisation Required"` (OCS) / `Basic realm="Nextcloud"` (DAV)
- [x] XHR detection uses `X-Requested-With: XMLHttpRequest` header — not a UA heuristic
- [x] Realm split: DAV paths (`/remote.php`, `/dav`, `/public.php`) → `"Nextcloud"`; OCS paths → `"Authorisation Required"` (REQ §4.3 vs §5.4)

**Verify:** `build/integration/features/auth.feature` — 401 header assertions. ✅ (implemented in auth middleware, Phase 3)

### 2.6 `OCS-APIREQUEST` CSRF bypass
- [x] When `OCS-APIREQUEST: true` is present, CSRF token check is skipped entirely
- [x] Without the header, CSRF check applies normally

**Verify:** POST to an OCS endpoint with `OCS-APIREQUEST: true` and no CSRF token → not rejected. ✅ (implemented in csrf.rs, Phase 3)

### 2.7 `GET /ocs/*/config`
- [x] Returns `version: "1.7"`, `website: "Nextcloud"`, `host` from request, `contact`, `ssl`
- [x] Available on both `/ocs/v1.php/config` and `/ocs/v2.php/config`

**Verify:** `build/integration/features/ocs-v1.feature` — `/config` response assertions. ✅ (smoke tested)

### 2.8 `GET /ocs/*/cloud/capabilities` — structure
- [x] Unauthenticated: returns only `IPublicCapability` subset
- [x] Authenticated: returns full merged capability set
- [x] Response ETag = `md5(json_encode($result))` (match PHP's encoding exactly)
- [x] `core`, `dav`, `files` capability blocks present with fields from REQ §5.6

**Verify:** `build/integration/capabilities_features/capabilities.feature` — all scenario assertions pass. ✅ (smoke tested; Phase 7 merges PHP-FPM app caps)

### 2.9 Capability cache
- [x] Payload built once at startup from DB and held in `Arc<RwLock<CapabilityCache>>`
- [x] Cache holds pre-serialised XML blob, JSON blob, and ETag for both authenticated and unauthenticated variants
- [x] Any write to `oc_appconfig` that affects a capability key rebuilds the cache
- [x] No DB query occurs for a `GET /cloud/capabilities` call on a warm cache

**Verify:** instrument with a counter: after startup, send 100 consecutive `GET /ocs/v2.php/cloud/capabilities` requests; assert `oc_appconfig` query count = 0. ✅

### 2.10 `POST /ocs/*/person/check` ~~CUT~~
> **Cut:** ownCloud federation compatibility endpoint. Not called by web, mobile, or desktop sync clients. Forward to PHP-FPM via Phase 7 catch-all.

### 2.11 `GET /ocs/*/identityproof/key/{cloudId}` ~~CUT~~
> **Cut:** ownCloud federation identity-proof endpoint. Same reasoning as 2.10 — forward to PHP-FPM.
