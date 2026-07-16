# Phase 11 — Native Preview / Thumbnail Fast Path

Goal: make gallery and grid-view loading fast on low-powered hosts by serving **already-generated** previews directly from Rust (zero PHP-FPM bootstrap per thumbnail), and by generating cache-misses through an **isolated** image backend — without reimplementing an image codec.

> **Why a dedicated phase.** On a 2-core host the dominant cost of a gallery scroll is **not** the pixel work — it is paying a full PHP-FPM bootstrap (autoloader, app manager, DB session, middleware) on **every** thumbnail request, including cache *hits*, and letting dozens of cache-misses generate at once thrash the CPU. Both are systemic problems the Rust front door is well placed to fix. The pixel work itself is left to libvips (via Imaginary or a subprocess pool); Rust does **not** try to out-compute it.

---

## Governing decisions (grounded)

- **Do not reimplement an image codec.** Imaginary is a thin HTTP wrapper around **libvips** (`lib/private/Preview/Imaginary.php` posts to `{url}/pipeline`). The Rust `image` crate would be slower and more memory-hungry than libvips, and matching libvips means binding to libvips anyway. The win from Rust is the **system around** the pixels (caching, concurrency, avoiding PHP bootstrap), not the resize kernel.
- **Keep generation out-of-process.** Image decoders are one of the most CVE-dense areas in software (e.g. libwebp `CVE-2023-4863`). Binding libvips **in-process via FFI** means one crafted upload can crash or corrupt the whole Rust server. Generation therefore runs behind an isolation boundary. The value Imaginary adds over raw libvips is exactly that isolation + ops maturity — **not** speed.
- **Pluggable generator backend.** The default backend is **Imaginary** (already a first-class Nextcloud provider, `preview_imaginary_url` / `preview_imaginary_key`). Keep it behind a trait so a self-hoster who does not want a second always-on service can later swap in a supervised pool of short-lived libvips subprocesses (`vipsthumbnail`) with the same isolation. Do **not** bind libvips in-process.
- **Storage model is DB-backed (Nextcloud 33).** Previews are **no longer** just files under `appdata_*/preview/<fileid>/<w>-<h>.<ext>`. They are rows in **`oc_previews`** (with bytes located via the file-cache appdata or, for object stores, **`oc_preview_locations`**), managed by `PreviewMapper` / `PreviewService` (`lib/private/Preview/Db/Preview.php`, `PreviewService.php`; table created in `core/Migrations/Version33000Date20250819110529.php`). A legacy folder-path layout still exists and is migrated on demand (`PreviewMigrationService::getInternalFolder`, config `enabledPreviewProviders` / `previewMovedDone`). **Any Rust cache-serve path must read `oc_previews`, not stat the appdata folder.**
- **Scope of generation in Rust:** raster images only (the Imaginary-supported set: `bmp, png, jpeg, gif, heic, heif, svg+xml, tiff, webp, illustrator` — `Imaginary::supportedMimeTypes()`). Video (ffmpeg), PDF/office/OpenDocument, and other delegate-heavy providers stay on PHP-FPM (Phase 7). Rust only fast-paths what libvips/Imaginary handles.

---

### 11.1 Compute `{nc:}has-preview` (prerequisite — shared with §10.12)
> PHP source: `apps/dav/lib/Connector/Sabre/FilesPlugin.php:392-393` — `has-preview` = `json_encode($previewManager->isAvailable($node))`.

- [ ] Replace the hardcoded `"false"` in `nc-dav/src/props.rs` with a value computed from the file mimetype against the enabled preview providers, gated on `enable_previews` (default on). Without this the web Files app never requests a thumbnail, so nothing else in this phase is observable.
- [ ] This item is tracked in both places; completing it here satisfies §10.12. Keep the two cross-referenced.

**Verify:** PROPFIND an image → `{nc:}has-preview` = `true`; a `.bin` → `false`. Web grid view begins issuing thumbnail requests for Rust-served files.

### 11.2 Serve cache **hits** natively (the primary win)
> PHP path served today: `/core/preview`, `/core/preview.png`, `/apps/files/api/v1/thumbnail/{x}/{y}/{file+}` (REQ §8, §12), backed by `Generator::getPreview()` → `PreviewService`/`PreviewMapper` lookup in `oc_previews`.

- [ ] Add a native handler for the thumbnail/preview routes that, for a requested `(fileid, width, height, crop, mode)`, looks up an existing preview via `oc_previews` (match on `file_id`, `width`, `height`, `cropped`, `mimetype`, `version` — mirroring `Generator::generatePreviews` `array_find`), reads the bytes from the resolved storage location (appdata for local, `oc_preview_locations` for object store — object store may be deferred, see out-of-scope), and streams them.
- [ ] Emit correct HTTP caching: strong `ETag` from the preview row's etag, `Cache-Control`, and `304 Not Modified` on `If-None-Match`; use async/zero-copy file streaming. This is the path that must avoid PHP-FPM entirely.
- [ ] On a cache **miss** (no matching row), hand off to 11.3/11.4 rather than blocking the serve path.

**Verify:** second load of a gallery (previews already generated) issues zero PHP-FPM requests for thumbnails; repeat requests return `304`; wall-clock gallery load on a 2-core host drops materially versus the PHP path.

### 11.3 Concurrency control + request coalescing
> PHP source: `lib/private/Preview/Generator.php` — `getNumConcurrentPreviews()` (`preview_concurrency_new`, default = hardware concurrency / fallback **4**; `preview_concurrency_all`, default = 2× / fallback **8**) guarded by SysV semaphores (`guardWithSemaphore` / `SEMAPHORE_ID_NEW` / `SEMAPHORE_ID_ALL`).

- [ ] Cap concurrent generations with a semaphore sized from the same config keys (`preview_concurrency_new` / `preview_concurrency_all`, same defaults), so a burst of misses on a 2-core box does not thrash the CPU. This replaces the per-worker SysV semaphore (which cannot coordinate across independent PHP-FPM workers) with a single in-process limiter — a strict improvement on low-core hosts.
- [ ] Coalesce duplicate in-flight requests: concurrent requests for the **same** `(fileid, size, crop)` await a single generation instead of each spawning one.

**Verify:** firing N ≫ ncores simultaneous cold thumbnail requests never runs more than the configured number of generations at once; duplicate concurrent requests for one preview trigger exactly one backend call.

### 11.4 Generate cache **misses** via a pluggable, isolated backend
> PHP source: `lib/private/Preview/Imaginary.php` — posts the source stream to `{preview_imaginary_url}/pipeline` with an `operations` array (`autorotate` / `convert` / `fit` or `smartcrop`, `width`/`height`/`quality`/`stripmeta`/`norotation`), `key = preview_imaginary_key`; honours `preview_format` (jpeg default, `webp` option), `preview_max_filesize_image` (default 50 MB), and quality from appconfig `preview.jpeg_quality` / `preview.webp_quality` (default 80).

- [ ] Define a `PreviewBackend` trait (`generate(source_stream, width, height, crop, mode, out_format) -> bytes`). Default impl = **Imaginary** HTTP client matching the PHP pipeline (same operations, format, quality, filesize cap, and `allow_local_address` semantics). Keep it out-of-process; do **not** link libvips into the server.
- [ ] Reuse PHP's **max-preview model**: generate/lookup the single "max" preview first, then derive smaller sizes from it (`Generator::getMaxPreview` + `calculateSize`), rather than re-decoding the original for each size.
- [ ] Fall back to PHP-FPM (Phase 7) for any mimetype outside the Imaginary-supported raster set (video/PDF/office/etc.), so behaviour is never worse than today.

**Verify:** a cold thumbnail for a JPEG/PNG/HEIC/WebP is generated through the backend and matches the PHP output size/format for the same request; an unsupported type (e.g. video) still yields a preview via the PHP fallback.

### 11.5 Persist generated previews (DB write — mind the sequence)
> PHP source: `PreviewMapper::insert` into `oc_previews` (`file_id`, `storage_id`, `width`, `height`, `cropped`, `mimetype_id`, `source_mimetype_id`, `mtime`, `size`, `max`, `etag`, …; `Db/Preview.php`), bytes stored via `StorageFactory`.

- [ ] After generating, store the bytes and insert the `oc_previews` row so future requests are cache hits, matching the columns/types PHP writes (so a subsequent PHP request also finds it). Preserve the `max` flag on the max preview.
- [ ] **Do not allocate the row id with `MAX(id)+1`** — same class of bug as §10.9. `oc_previews`/`oc_preview_locations` ids are DB-managed (`Version33000Date20251023110529` explicitly *removes* auto-increment from `preview_locations.id`, so match whatever scheme PHP uses per table); use the DB/`SnowflakeAwareEntity` id path so Rust- and PHP-written previews never collide.
- [ ] Respect `enable_previews` / `enabledPreviewProviders` gating before writing.

**Verify:** a preview generated by Rust is found and served by a subsequent **PHP** request (row + bytes are PHP-compatible); ids do not collide with PHP-inserted previews under interleaved load.

### 11.6 (Stretch) Background pre-generation
- [ ] Optionally warm the max preview for newly uploaded previewable files (on the Rust upload path or via a queue) so the first gallery view is mostly cache hits. Keep it bounded by the 11.3 concurrency limiter and off the request path.

**Verify:** after uploading a batch of images, the first gallery load is predominantly cache hits (few/no cold generations).

---

### Out of scope (intentional)

- **In-process libvips FFI** — rejected for crash-safety (a malformed image would take down the whole server). Generation stays out-of-process (Imaginary or a subprocess pool). Revisit only with hard sandboxing.
- **Object-store preview locations** — `oc_preview_locations` / `StorageFactory` object-store path may be deferred initially; local-disk appdata is the first target (consistent with the Phase 10 local-disk storage assumption). Cache-serve for object-store-backed previews can fall back to PHP-FPM until implemented.
- **Non-raster providers** — video (ffmpeg), PDF/office/OpenDocument, and other delegate-heavy providers stay on PHP-FPM. Rust only fast-paths the Imaginary-supported raster set.
- **Legacy folder-path previews** — the on-demand migration from the old `appdata` layout to `oc_previews` (`PreviewMigrationService`) remains a PHP concern; Rust reads the migrated `oc_previews` rows.
