# 10 — Photos grid previews stay PHP-FPM-bound and serialize even hot cache hits behind `preview_concurrency_all`

**Status:** root-caused 2026-09-03; native Photos cache-hit fast path implemented in this
commit (deploy + A/B pending). **Related:** [note 03](03-memories-multipreview-php-bound.md) (the same
PHP-bound gallery problem, memories flavour), [Phase 11](../04-tasks/phase-11.md)
(native preview fast path), [plan 14](../03-implementation-plan/plan/14-native-preview-thumbnail-fast-path.md).

### What happened

The Photos grid on the deployment under test shows no thumbnails for 1–5 s, then all
of them appear at once — including on repeat loads where every thumbnail is already
generated ("hot"). A page load fires one request per visible photo:

```
GET /index.php/apps/photos/api/v1/preview/{fileId}?x=64&y=64
GET /index.php/apps/photos/api/v1/preview/{fileId}?x=1024&y=1024
```

One captured load issued ~50 `x=1024` requests within ~4 s. Server-side latencies were
hundreds of milliseconds per request even after the previews were cached, and the whole
batch finished together — which is why the client paints them at once.

### Root cause

1. **The Photos app preview API is not on the Rust-native surface.** The native Phase 11
   fast path covers `/core/preview`, `/core/preview.png`, and
   `/apps/files/api/v1/thumbnail/{x}/{y}/{*file}` (`nc-server/src/router.rs`, native route
   block). `/index.php/apps/photos/api/v1/preview/{fileId}` is not registered there, so it
   falls through the `/index.php/{*path}` → `php_fpm_fallback` route
   (`router.rs:484`). Every thumbnail therefore pays a full PHP-FPM request even when the
   bytes are already on disk.
2. **PHP's controller does real work per request.** `Photos\PreviewController::index`
   resolves the file through the user folder, checks shared-storage/download
   permissions, tries album lookups when the file is not a direct home-storage node, and
   only then calls `IPreview::getPreview` (`apps/photos/lib/Controller/PreviewController.php`,
   `index`/`fetchPreview`).
3. **Even hot cache hits serialize.** `PreviewManager::getPreview` acquires the global
   `SEMAPHORE_ID_ALL` semaphore before calling `Generator::getPreview`
   (`lib/private/PreviewManager.php:176-181`); the semaphore's concurrency is
   `preview_concurrency_all` (`Generator.php:292-317`). The cache lookup happens *inside*
   the guarded section, so "is it cached?" is answered at most
   `preview_concurrency_all` requests at a time. With the restrictive values deployed,
   ~50 requests × hundreds of ms ÷ 2 concurrent ≈ the observed multi-second batch tail.
4. **The client waits for the batch.** The Photos grid renders when the visible batch
   resolves, so the slowest request determines the perceived time and every thumbnail
   appears simultaneously. This is a frontend rendering choice, not a transfer problem;
   network throughput and disk I/O were both far from saturation during the measurement.

### Options weighed

- **Raise `preview_concurrency_all` / `preview_concurrency_new`.** Cheap, reversible, and
  it shortens the tail, but every request still boots PHP, so the per-request floor
  (~30–80 ms on this stack for trivial PHP routes, more for previews) remains, and the
  cap must stay low enough to protect a small host during cold generation. It fixes the
  queue, not the per-request cost.
- **Native Photos cache-hit route (chosen).** Extend the existing Phase 11.2 pattern to
  `/index.php/apps/photos/api/v1/preview/{fileId}` and `/apps/photos/api/v1/preview/{fileId}`:
  serve only when the file resolves to the caller's own home storage *and* a matching
  preview row exists; proxy every other case to PHP-FPM. This is the same IDOR-safe
  contract already proven for `/core/preview`, and it removes the PHP boot + semaphore
  from the dominant own-photos gallery case.
- **Batch endpoint / frontend change.** A batch response (like memories `multipreview`)
  would cut request count but changes an app-specific unversioned contract and needs a
  frontend patch; out of scope for the immediate fix.

### The choice

Implement the native Photos preview hit path with the existing `serve_preview` machinery
(route-kind `Core`: `cacheFor(86400, private, immutable)`, 304 semantics,
`Content-Disposition`). Non-home files, unauthenticated requests, misses, and all error
cases proxy to PHP-FPM so behavior is never worse than PHP. The Photos controller is
`@NoCSRFRequired`, so the route is deliberately **not** added to the edge SameSite-gated
prefix list even though it is native.

### Verification

- Deployed binary route strings contain `/index.php/core/preview` and
  `/index.php/apps/files/api/v1/thumbnail/...` but no `/apps/photos/api/v1/preview`,
  confirming the pre-change surface.
- Live capture: ~50 Photos preview requests in ~4 s; p50 ≈ 245 ms, p95 ≈ 1.3 s for the
  preview-ish batch; FPM active workers approached the pool ceiling; CPU and disk were
  not saturated.
- HAR comparison (one warm `x=64&y=64` request): the PHP response's status, content
  type, content length, `cache-control: private, max-age=86400, immutable`, quoted
  `ETag`, `content-disposition: inline; filename="<row-name>"`, `expires` (+24 h), and
  the preview-specific headers all match the native builder. The remaining framework
  headers (`X-Request-Id`, `X-User-Id`, CSP, Feature-Policy, and the `base.php`
  security set) are **not** injected by the preview handler — see the follow-up below.
  Behind the production nginx, nginx adds its own copies of the security headers, so
  the duplicates seen in the HAR are reproduced.
- After the change: cache-hit Photos requests should be served by Rust (single indexed
  query + file stream, no PHP), with misses/errors still proxied. A/B verification and a
  differential scenario are follow-ups below.
- Follow-up heavy-scroll capture after deploying the native route: 760 preview
  requests in the 3-minute window; 231 answered in ≤10 ms and 288 in ≤50 ms
  (native cached hits). The remaining slow tail was dominated by `x=64` grid
  tiles (avg ≈1 s, max 4.2 s) that had no cached row yet and therefore proxied to
  PHP-FPM generation; FPM reached `max_children`, host load peaked ≈4.9 on 4
  cores, while disk reads stayed <3 MB/s and network was negligible. Cached
  `x=1024` requests were mostly ≤10 ms.
- Per-request proxy proof (warm private-mode load after generation): with
  `RUST_LOG=nc_fastcgi=debug,info`, 2,942 preview requests were logged and
  **zero** `proxy_handler` FastCGI spans appeared — every preview was served by
  the Rust native path (p50 8 ms, p90 55 ms, p95 91 ms, max 216 ms). The debug
  logging was temporary and has been reverted.

### Follow-ups

- Add a `difftest` scenario replaying Photos grid loads (own-home files, hot previews)
  asserting zero PHP-FPM requests for the native hit set and byte/header parity with PHP.
- Re-measure the same grid load after deploy; target is sub-second batch completion for a
  fully warm gallery with no FPM involvement.
- **Shared framework-response headers (deferred):** PHP emits `X-Request-Id`,
  `X-User-Id`, `Content-Security-Policy`, `Feature-Policy`, and the `base.php`
  security headers globally (`Response::getHeaders`, `base.php:635-643`), not from the
  Photos controller. A native handler should not duplicate them per route; the follow-up
  is a shared response-header layer (or helper) applied to all native routes, then the
  HAR parity check can be completed for the preview path. Until then, preview responses
  intentionally stay lean and the missing framework headers are the known gap.
- Consider the same treatment for other first-party gallery preview APIs
  (`/index.php/apps/photos/api/v1/publicPreview/{fileId}` is a separate shared/public
  surface and must stay on PHP until share-authz is fast-pathed).
- The remaining gallery choke is cold `x=64` variant generation, not the native
  hit path. Options: pre-generate grid-sized previews in the background, or land
  native bucketed-variant generation (Phase 11.4) so misses are also served
  without a PHP-FPM round trip. Raising `pm.max_children` alone is not the
  answer here — the host already peaked at ~4.9 load on 4 cores during
  generation.
