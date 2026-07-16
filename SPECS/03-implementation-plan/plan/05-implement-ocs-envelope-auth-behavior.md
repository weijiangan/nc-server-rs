## 2) Implement OCS envelope + auth behavior

### Coding steps
1. Implement OCS response wrappers for v1 and v2:
	- v1: HTTP 200 for most responses, OCS `statuscode=100` for success.
	- v2: preserve HTTP status mappings (`401`, `404`, `500`, etc.).
2. Implement format negotiation (`?format=` + `Accept` header) with XML default.
3. Implement unauthorized behavior with `WWW-Authenticate` and XHR `DummyBasic` variant.
4. Implement core OCS endpoints:
	- `/config` — returns `version: "1.7"` (not 1.8)
	- `/cloud/capabilities` — authenticated requests return full capabilities; unauthenticated return only `IPublicCapability` results; ETag = `md5(json_encode($result))`
	- `/person/check` — **cut:** forward to PHP-FPM via Phase 7 catch-all (ownCloud federation compatibility only; not called by web/mobile/desktop sync)
	- `/identityproof/key/{cloudId}` — **cut:** same reasoning; forward to PHP-FPM
5. Implement CSRF exceptions for OCS (`OCS-APIREQUEST: true`) and bearer-token bypass semantics.
6. Cache the capability payload: build it once at startup and after any `oc_appconfig` write that affects capabilities. Store as `Arc<RwLock<CapabilityCache>>` holding the pre-serialised XML and JSON blobs with their ETag. Capability probes (the most frequent unauthenticated request from sync clients) never touch the DB after the first build.

### Verification steps
1. Reuse existing OCS/integration suites:
	- `build/integration/features/ocs-v1.feature`
	- `build/integration/capabilities_features/capabilities.feature`
	- `build/integration/features/auth.feature`
2. Add assertion parity checks for:
	- OCS envelope fields (`status`, `statuscode`, `message`, `data`)
	- content type (`text/xml; charset=UTF-8`, `application/json; charset=utf-8`)
	- auth headers on 401.

---

---

Prev: [`04-stand-up-the-http-server-skeleton.md`](04-stand-up-the-http-server-skeleton.md) · Up: [`README.md`](README.md) · Next: [`06-implement-dav-service-routing-auth-stack.md`](06-implement-dav-service-routing-auth-stack.md)
