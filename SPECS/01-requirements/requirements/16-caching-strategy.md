## 16. Caching Strategy

### 16.1 In-process cache (Rust `Arc<RwLock<…>>`)

| Cache | Invalidation trigger |
|---|---|
| Route registry (app route map) | App enable/disable, config reload |
| Capabilities payload | Config change in `oc_appconfig`, app enable/disable |
| Auth token hot cache (token hash → uid) | Token revocation or expiry; TTL ≤ 5 minutes |
| Mime type map (`oc_mimetypes`) | Table change (rare; startup + periodic) |
| App config values (`oc_appconfig`) | Write to the same key |
| User quota values | Write to quota config key |

### 16.2 Distributed cache (PHP compat)

Chunked upload v2 metadata **requires** a distributed cache (Redis or Memcached) configured in Nextcloud config as `memcache.distributed`. Rust must use the same cache backend. If not configured, chunked upload v2 must be disabled (capability `dav.chunking` still reported as `1.0` for v1 compat).

### 16.3 Preview response caching

Native preview responses (`/core/preview`, `/core/preview.png`) carry `Cache-Control: private, max-age=86400, immutable` + `Expires` (+24 h), a strong `ETag` from the `oc_previews.etag` column (the source file's etag at generation), and `Last-Modified` from the row's generation timestamp; `304` on `If-None-Match`/`If-Modified-Since`. Staleness is maintained by deleting a file's preview rows on content write (Phase 11.5, mirroring PHP's `Preview\Watcher`), never by revalidating against the source at read time. The deprecated `/apps/files/api/v1/thumbnail/…` route keeps PHP's default `no-cache, no-store, must-revalidate` headers.

---

---

Prev: [`15-security-headers.md`](15-security-headers.md) · Up: [`README.md`](README.md) · Next: [`17-logging-and-observability.md`](17-logging-and-observability.md)
