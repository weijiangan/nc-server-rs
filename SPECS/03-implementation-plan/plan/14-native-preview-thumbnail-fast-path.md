# 9) Native Preview / Thumbnail Fast Path — Implementation Plan

**Status:** design complete, not started · **Execution tracking:** [`../../04-tasks/phase-11.md`](../../04-tasks/phase-11.md) · **Requirements:** REQ §2.1, §6.5, §8.1, §9.10, §16.3, §18

## 1. Purpose

On a 2-core host, gallery scrolling is strangled by two systemic costs: a full PHP-FPM bootstrap on **every** thumbnail request (including cache *hits*), and uncapped concurrent generation on cache *misses*. This plan moves the preview serve path into Rust:

- **Hits** are served natively — one indexed DB lookup + zero-copy file stream. Zero PHP.
- **Misses** for libvips-handleable raster types are generated through **Imaginary** (out-of-process libvips), with in-process admission control and request coalescing — things PHP's shared-nothing model cannot do.
- **Everything else** falls back to PHP-FPM, so behaviour is never worse than today.

Rust does not reimplement an image codec and does not bind libvips in-process (one crafted upload must not be able to crash the server).

### Success criteria

1. Second gallery load (warm previews) issues **zero** PHP-FPM requests for thumbnails; repeat requests return `304`.
2. Rust-generated preview rows + bytes are served correctly by a subsequent **PHP** request, and vice versa (full bidirectional interop).
3. N ≫ ncores concurrent cold requests never exceed the configured generation concurrency; duplicate in-flight requests trigger exactly one backend call.
4. Byte- and header-identical responses versus the PHP path for the same request on a hit.
5. Wall-clock gallery load on a 2-core host drops materially vs PHP (measured in Phase 8 harness).

---

## 2. Ground truth (PHP, NC 33)

All verified against `workspace/server/`; cited inline. The canonical task-level detail lives in `phase-11.md` — this section is the design summary.

**Storage model.** Previews are rows in `oc_previews` (REQ §9.10), bytes at `{datadir}/appdata_{instanceid}/preview/{md5(file_id)[0..7], each char a nested dir}/{file_id}/{version-}{w}-{h}[-crop][-max].{ext}` (`LocalPreviewStorage::constructPath:81-83`). Ids are **client-side snowflakes** — autoincrement was removed from all three preview tables (`Version33000Date20251023110529`).

**Serve flow** (`Generator::generatePreviews`, `Generator.php:104-204`): fetch all rows for `file_id` → find/generate the **max preview** (`max=true`, ≤ `preview_max_x/y`, default 4096²) → snap the requested size with `calculateSize` *relative to the max preview's actual dimensions* → exact match returns the max row → else `array_find` on `(width, height, cropped, mimetype=output-mime-of-max, version)` → else derive from the max image and persist. `mtime`/`etag` are never consulted at read; zero-size results are deleted and reported as `NotFoundException`.

**Staleness.** `Watcher::postWrite` deletes **all** of a file's preview rows + bytes on any content write (`Watcher.php:36-61`). Deletion is what invalidates — there is no revalidation. Orphans from deletes are swept hourly by `BackgroundCleanupJob`.

**Generation backend.** `Imaginary.php:44-155` — pipeline `POST {url}/pipeline?operations=<json>&key=<key>`; op1 `autorotate` | `convert{type}` (svg/pdf/illustrator→png) | neither (heic); op2 `fit`|`smartcrop` with `{width, height, stripmeta:"true", type, norotation:"true", quality}`. Source streamed as body with the source's content-type; timeouts 120 s / connect 3 s; `allow_local_address: true`. Output mime: source-mapped (gif/png/svg/pdf/ai→png; jpeg/bmp/x-bitmap/tiff/webp/heif/heic→jpeg), one-way `preview_format=webp` override; quality from appconfig `preview/jpeg_quality|webp_quality` (default 80). Source cap `preview_max_filesize_image` (default 50 MiB, `-1` unlimited) checked before POST.

**Concurrency.** Two system-wide SysV semaphores (`Generator.php:230-242, 295-317`): ALL (`preview_concurrency_all`, 2× cores / fallback 8) wraps the whole request incl. lookup (`PreviewManager.php:176-177`); NEW (`preview_concurrency_new`, cores / fallback 4) wraps only the provider call and resize work.

**Provider gating.** Every core provider — *including Imaginary* — is registered only if its class is in the system config `enabledPreviewProviders` (default: `MarkDown, TXT, OpenDocument, PNG, JPEG, GIF, BMP, XBitmap, Krita, WebP`; `PreviewManager.php:260-290`).

**Events.** `BeforePreviewFetchedEvent` has exactly one listener: `admin_audit` (audit log line). See §13 deviations.

---

## 3. Architecture

```
GET /core/preview?fileId=…          GET /core/preview.png?file=…       GET /apps/files/api/v1/thumbnail/x/y/file
                │                                │                                  │
                └────────────────────────────────┴──────────────────────────────────┘
                                                 ▼
                              nc-server route → nc-preview::handlers
                                                 │
                            1. authz: resolve node in user folder (path or fileId),
                               PERMISSION_READ + hide-download (x-nc-preview) check
                                                 │
                            2. param validation (400s) · enable_previews gate (404)
                                                 │
                            3. ProviderRegistry::generatable(source_mime)? ── no ──┐
                                                 │ yes                             │
                            4. store::lookup(file_id) → match(w,h,crop,version)    │
                                                 │ hit                             │
                            5. stream bytes + headers (ETag/304/…)                 │
                                                 │ miss                            │
                            6. Backend available? (Imaginary URL + gated) ── no ───┤
                                                 │ yes                             │
                            7. coalesce(key) → semaphore(NEW) →                    │
                               max preview (generate or row) →                     │
                               derive bucketed size → persist row + bytes          │
                                                 │                                 │
                            8. stream result                                       │
                                                                                   ▼
                                                            nc-fastcgi proxy to PHP-FPM
                                                            (Phase 7 — miss fallback,
                                                             non-raster, object store)
```

**Cross-cutting hook:** the Rust PUT-overwrite path (`nc-dav` filesystem/upload handlers) calls `nc-preview::invalidate(file_id)` after a successful content write — Watcher parity (§8).

### Module layout — new crate `nc-preview`

Preview logic is consumed by HTTP handlers (nc-server) *and* the DAV write path (nc-dav) and must not create a cyclic dependency between them. A leaf crate both can depend on:

```
core-rs/crates/nc-preview/
├── src/
│   ├── lib.rs            # PreviewService facade: get_preview(node, spec) -> ServedPreview
│   ├── config.rs         # config surface (§9); PreviewConfig snapshot
│   ├── registry.rs       # ProviderRegistry — availability/gating, shared with has-preview
│   ├── size.rs           # calculateSize (PHP-exact bucketing)
│   ├── store.rs          # oc_previews queries, byte paths, row insert/delete
│   ├── snowflake.rs      # SnowflakeGenerator (PHP-exact bit layout)
│   ├── backend.rs        # PreviewBackend trait + Imaginary client
│   ├── coalesce.rs       # in-flight request map + semaphore
│   ├── invalidate.rs     # Watcher-parity deletion
│   └── handlers.rs       # axum handlers for the three routes (+ PHP-proxy handoff)
```

Depends on: `nc-db` (pool, config, mimetype map), `nc-fastcgi` (fallback proxy). `nc-dav` depends on `nc-preview` only for `invalidate()` and consumes `registry.rs` for `{nc:}has-preview` (replacing the private matrix currently in `nc-dav/src/preview.rs`). `nc-server` wires the routes and constructs the shared `Arc<PreviewService>`.

---

## 4. Component design

### 4.1 ProviderRegistry — single source of truth for availability

Built once at startup from config (REQ §16.1 in-process cache; rebuild on config change):

```rust
pub struct ProviderRegistry {
    enabled: HashSet<ProviderClass>,      // from enabledPreviewProviders, PHP default list when unset
    ffmpeg: bool,                          // preview_ffmpeg_path set (or PATH-found — see risk R5)
    libreoffice: bool,
    imaginary: bool,                       // preview_imaginary_url valid AND Imaginary ∈ enabled
}

impl ProviderRegistry {
    /// PHP PreviewManager::isAvailable — drives {nc:}has-preview (§10.12/§11.1)
    pub fn is_available(&self, mime: &str) -> bool;
    /// Strict subset: can *Rust* generate this natively? (Imaginary-supported set ∩ gated ∩ backend up)
    pub fn rust_generatable(&self, mime: &str) -> bool;
}
```

The property and the generator can never disagree: `rust_generatable(m) ⇒ is_available(m)`. HEIC/HEIF: `is_available` is true when Imaginary is gated in (Imaginary handles HEIC without imagick), false otherwise (Rust cannot introspect a PHP imagick build).

### 4.2 Size negotiation — `calculateSize` parity

PHP-exact port (`Generator.php:420-496`), f64 throughout, `round()` at the end:

```
calculate_size(w, h, crop, mode, max_w, max_h) -> (u32, u32):     # max_* = the MAX PREVIEW'S actual dims
  if !crop:
    ratio = max_h / max_w
    if w == -1: w = h / ratio
    if h == -1: h = w * ratio
    (ratio_h, ratio_w) = (h / max_h, w / max_w)
    if mode == FILL:  { if ratio_h > ratio_w: h = w*ratio  else: w = h/ratio }   # request = outer box
    if mode == COVER: { if ratio_h > ratio_w: w = h/ratio  else: h = w*ratio }   # request = inner box
  if h != max_h && w != max_w:
    p4h = max(4^ceil(log4(h)), 64);  p4w = max(4^ceil(log4(w)), 64)               # snap UP to power of 4
    (ratio_h, ratio_w) = (h / p4h, w / p4w)
    if ratio_h < ratio_w: { w = p4w; h /= ratio_w } else { h = p4h; w /= ratio_h }
  clamp w/h to (max_w, max_h) preserving ratio;  return (round(w), round(h))
```

Plus the surrounding flow rules from `generatePreviews`: `w == -1 && h == -1` → serve the max row as-is; bucketed == max dims → serve the max row. Golden vectors are ported from running PHP (§11).

### 4.3 Serving hits

**Authz matrix** (before anything else):

| Check | `/core/preview*` | files thumbnail |
|---|---|---|
| Node resolution | `fileId` ∈ user folder (`getFirstNodeById` semantics) / path | user-relative path |
| Not found / not a file | 404 (empty) | 404 `{"message":"File not found."}` |
| Not readable (`PERMISSION_READ`) | **403** | **404** (PHP maps NotPermitted→404) |
| Hide-download share, no `x-nc-preview: true` header | **403** | **404** |

Resolution reuses nc-dav's existing filecache/share machinery (`oc_share.hide_download` — Rust already owns it, REQ §9.6).

**Lookup** (single query, index on `file_id`):

```sql
SELECT id, width, height, mimetype_id, source_mimetype_id, max, cropped, etag, mtime, size, version_id
FROM oc_previews WHERE file_id = $1
```

Match in memory: `width ∧ height ∧ cropped ∧ version_id ∧ mimetype_id == max_row.mimetype_id` (output mime of the max row, per `Generator.php:168-170`). Instances configured for object storage skip native serving entirely (boot-time check; §12).

**Response** — hit path is the hot path: `tokio::fs` + `sendfile`-style streaming (no buffering), headers:

| Header | Value |
|---|---|
| `Content-Type` | row output mimetype |
| `ETag` | `"<row.etag>"` (source file's etag at generation) |
| `Last-Modified` | RFC 1123 of `row.mtime` (**generation** time) |
| `Cache-Control` | core routes: `private, max-age=86400, immutable` + `Expires` (+24 h) · files route: `no-cache, no-store, must-revalidate` |
| `Content-Disposition` | `inline; filename="<name>"` |
| `X-Robots-Tag` | `noindex, nofollow` |

`304` on `If-None-Match` **and** `If-Modified-Since` (PHP `NotModifiedMiddleware`).

### 4.4 Generation — max-first, Imaginary-backed

```rust
pub struct RenderSpec { pub w: u32, pub h: u32, pub crop: bool, pub out_mime: OutMime, pub quality: u8 }
pub struct Rendered   { pub bytes: Bytes, pub width: u32, pub height: u32, pub mime: String }

#[async_trait]
pub trait PreviewBackend: Send + Sync {
    /// First-time decode of the ORIGINAL (autorotate/convert apply).
    async fn render_source(&self, src: BoxedStream, src_mime: &str, spec: &RenderSpec) -> Result<Rendered, GenError>;
    /// Resize of already-sanitized max-preview bytes (fit/smartcrop only).
    async fn render_from_max(&self, max: BoxedStream, spec: &RenderSpec) -> Result<Rendered, GenError>;
}
```

`ImaginaryClient` implements both as one pipeline builder differing only in op1. The default (and only, for now) impl; a `vipsthumbnail` subprocess pool can implement the same trait later.

**Miss flow** (`PreviewService::generate`):

1. Max row present (`max=true, version_id=-1`)? → use it. Else acquire NEW permit → stream the source file to Imaginary at `(preview_max_x, preview_max_y)` fit → persist max row (`max=true`) + bytes → release.
2. `calculate_size(requested, max_row dims)` → equals max dims → done.
3. Acquire NEW permit → `render_from_max(max bytes, bucketed)` → persist derived row (`max=false`) + bytes.
4. Size-0 result → delete row + bytes, 404 (PHP parity). `GenError::InvalidImage` → 400 on the files route / 500 on core routes (mirrors PHP's per-controller exception handling — `ApiController` catches `\Exception`→400, `PreviewController` only maps NotFound/InvalidArgument).
5. **Client disconnect does not cancel generation** — the generation future is a detached `tokio::spawn` (bounded by the semaphore); waiters observe it via `Shared`. Gallery scrolling cancels requests constantly; completing the work warms the cache for the next scroll. This is a deliberate improvement over PHP.

Source cap (`preview_max_filesize_image`) checked before POST; oversize → 404 without touching the backend.

### 4.5 Concurrency + coalescing

- `tokio::sync::Semaphore` with `preview_concurrency_new` permits (cores, fallback 4). Held across each Imaginary call (max and derived). The ALL semaphore is not replicated — hits need no admission control in Rust (that is part of the win: PHP admits hits too because hits still pay bootstrap).
- Coalescing: `Mutex<HashMap<CoalesceKey, Shared<JoinHandle<Result<Arc<Served>, GenError>>>>>`, `CoalesceKey = (file_id, w, h, crop, version_id)` post-bucketing (output mime is a deterministic function of source mime + `preview_format`). A spawned janitor removes the key once the shared future resolves; late arrivals after resolution hit the DB row. Duplicate concurrent requests → exactly one backend call; all waiters get the same result **or the same error → each falls back to the PHP-FPM proxy** (PHP may still succeed via GD/other providers).
- **Honest tradeoff:** while any generation falls back to PHP, global concurrency = Rust cap + PHP cap (the SysV semaphore and the tokio semaphore share no state). Ops guidance: size `preview_concurrency_new` ≈ half the desired global cap, or future-work Rust-side `sysvsem` participation.

### 4.6 Persistence

**Snowflake ids** (`lib/private/Snowflake/SnowflakeGenerator.php`, verified):

```
id = (seconds - 1759276800) << 32
   | (ms & 0x3FF)       << 22
   | (serverid & 0x1FF) << 13
   | (is_cli & 1)       << 12
   | (sequence & 0xFFF)
```

`serverid` = system config `serverid` if > 0 else `crc32(hostname)`; `is_cli = 0` for Rust; 12-bit sequence per (ms, serverid), spin to next ms on overflow. **Verify R1 before M2** (PHP's sequence may be shared across workers via `FileSequence`/`APCuSequence` — see risks).

**Column semantics** (traps marked): `etag` = **source file's etag at generation** (not bytes' etag); `mtime` = **generation timestamp** (not file mtime); `encrypted=false`; `version_id=-1` (local disk is unversioned); `storage_id` = mount numeric id; `width/height` = actual produced pixels; `size` after the byte write; `old_file_id`/`location_id` NULL.

**Mimetype ids** via the nc-db mimetype map (REQ §16.1) with **get-or-create** on miss (PHP `MimetypeLoader::getId` inserts; the unique `mimetype` column makes this race-safe).

**Insert with collision handling** — the unique index `(file_id, width, height, mimetype_id, cropped, version_id)` exists for cross-writer races (PHP ↔ Rust, Rust ↔ Rust):

```sql
-- PG:   INSERT … ON CONFLICT (file_id, width, height, mimetype_id, cropped, version_id) DO NOTHING RETURNING id
-- MySQL: INSERT IGNORE …;  SQLite: INSERT OR IGNORE …;  → 0 rows ⇒ SELECT by the same key
```

On conflict, serve the existing row (PHP `Generator::getMaxPreview:338-345` does the same).

**Bytes:** construct the md5-sharded path (§2), create dirs, write-then-rename for atomicity (a half-written file must never be visible to a PHP reader), then INSERT the row with `size`. Row-after-bytes ordering: a reader that sees the row always finds complete bytes; the inverse (bytes without row) is swept by PHP's cleanup job.

### 4.7 Invalidation — Watcher parity (correctness-critical)

Rust's PUT-overwrite is native, so PHP's `postWrite` hook never fires. Without this, overwritten files serve stale previews forever (and the stale row's ETag keeps 304-matching clients' cached copies).

- **Where:** after a successful content write in `nc-dav` — simple PUT overwrite and chunked-upload assembly (and bulk upload per-file). Fire-and-forget `tokio::spawn`, **never** on the response critical path.
- **What:** `SELECT id, width, height, cropped, max, mimetype_id, version_id FROM oc_previews WHERE file_id=$1` → `DELETE FROM oc_previews WHERE file_id=$1` → unlink each byte path. Rows deleted first (no new hits can start), bytes best-effort — failures logged at `warn!` (CLAUDE.md hygiene rule 1: silent failure here is invisible; PHP's hourly orphan sweep is the backstop).
- **Not on MOVE/COPY** (content unchanged; PHP doesn't either). **Not on DELETE/trash** (PHP defers to the hourly job; trashed files keep their filecache row). **Version restore** needs nothing (PHP-FPM side; PHP's own listener fires).
- **Open verification:** whether PHP fires `postWrite` for `X-OC-MTime`-only touches — match whatever it does.

### 4.8 Fallback matrix

| Condition | Behaviour |
|---|---|
| Row match | Serve natively (always — pure read, no gating beyond authz) |
| Miss, `rust_generatable(mime)`, Imaginary configured + gated | Generate natively |
| Miss, backend errors (`GenError`) | PHP-FPM proxy (PHP's provider chain may still succeed) |
| Miss, non-raster / Imaginary not configured / not gated | PHP-FPM proxy |
| Instance on object storage | Proxy the whole route to PHP-FPM (boot-time decision) |
| `enable_previews = false` | 404 (PHP `NotFoundException('Previews disabled')`) |
| `mimeFallback=true`, no preview | 303 → PHP `/core/mimeicon` URL (or full proxy — interim) |
| Legacy unmigrated bytes exist but no rows | Miss → generate fresh (Rust never reads the legacy layout; PHP migration remains PHP's concern) |

---

## 5. Endpoint specification

| | `/core/preview` | `/core/preview.png` | `/apps/files/api/v1/thumbnail/{x}/{y}/{file+}` |
|---|---|---|---|
| PHP source | `PreviewController::getPreviewByFileId` (`:107`) | `PreviewController::getPreview` (`:63`) | `ApiController::getThumbnail` (deprecated 32.0.0, hardcodes `crop=true`) |
| Params | `fileId=-1, x=32, y=32, a=false, forceIcon=true, mode=fill, mimeFallback=false` | `file='', …same` | `x, y, file` only |
| 400 | `fileId=-1` / `x=0` / `y=0` (empty) | `file=''` / `x=0` / `y=0` (empty) | `x<1 \| y<1` → `{"message":"Requested size must be numeric and a positive value."}` |
| 404 | not found / no provider / disabled (empty body) | same | `{"message":"File not found."}` |
| Generation failure | 400 (`InvalidArgument`), else 500 | same | 400 empty (catches `\Exception`) |
| Crop mapping | `crop = !a` | `crop = !a` | `crop = true` always |

`a` = preserve aspect ratio; `forceIcon` only matters with `mimeFallback` (icon path stays PHP); `mode ∈ {fill, cover}`.

---

## 6. Configuration surface

All in REQ §18 (canonical): system config `enable_previews`, `enabledPreviewProviders`, `preview_imaginary_url` (**sensitive**), `preview_imaginary_key` (**sensitive** — never in logs, even at debug), `preview_concurrency_new`, `preview_max_x/y`, `preview_format`, `preview_max_filesize_image`, `preview_ffmpeg_path`, `preview_libreoffice_path`, `serverid`; appconfig (`oc_appconfig`, appid `preview`): `jpeg_quality`, `webp_quality`. Loaded into a `PreviewConfig` snapshot at startup alongside the existing §10.12 fields in `nc-db`; `enabledPreviewProviders` replaces the private matrix in `nc-dav/src/preview.rs`.

---

## 7. Migration

One new sqlx migration (per-DB dialects, following `core-rs/migrations/` convention) creating `oc_previews`, `oc_preview_locations`, `oc_preview_versions` exactly per REQ §9.10 — including the unique index. Additive-only no-op on PHP-created DBs (§9.7: PHP Doctrine already created them; existence-guarded DDL). Fresh installs get them from Rust.

---

## 8. Security

- **Authz first, always** (§4.3): the fileId endpoint is an IDOR surface without user-folder-scoped resolution; the hide-download `x-nc-preview` check prevents share-link content leakage via preview URLs.
- **Secrets:** `preview_imaginary_url`/`_key` are sensitive (PHP `SystemConfig.php:42-43`) — redact from all log lines and error responses (REQ §17).
- **SSRF:** loopback is allowed *only* on the Imaginary client (Imaginary normally runs on localhost) — the allowance is scoped to that one `reqwest::Client`, never global.
- **DoS:** source size cap before POST; semaphore bounds backend concurrency; generation is bounded work per request (bucketed sizes, one max + one derived). Decoder CVEs stay behind the isolation boundary by design.

---

## 9. Observability

Per REQ §17: `debug!` per request — `preview hit/miss file_id=… size=…`, coalesce-join, semaphore wait time; `info!` generation completion with backend latency; `warn!` invalidation failures, Imaginary errors, snowflake sequence overflow; `error!` DB failures on persist. Request id propagated (`X-Request-Id`). Metrics candidates (if a metrics sink lands): hit rate, generation latency histogram, semaphore saturation, coalesce dedup rate.

---

## 10. Test strategy

**Unit** (named per task in phase-11 §11.1–11.5): bucketing, row matching, pipeline JSON per source mime, snowflake layout, path construction, overwrite invalidation, fallback decisions.

**Golden vectors from running PHP** — the ground truth is what PHP does:
- `calculateSize`: run a reflection script against `workspace/server` (`Generator::calculateSize` is private) over a corpus of `(w, h, crop, mode, maxW, maxH)` → assert Rust outputs identical pairs.
- Imaginary pipeline: point `preview_imaginary_url` at a logging stub, request a preview per source type from PHP, capture the `operations` JSON; Rust against the same stub must emit byte-identical query strings.

**Interop (live PG + Imaginary container):** Rust-generated row → `curl` the PHP endpoint for the same `(file, x, y)` → 200, same pixels; reverse direction; interleaved Rust/PHP inserts under load → no id or unique-key collisions; Rust PUT overwrite → PHP-served thumbnail updates.

**Load (Phase 8 harness extension):** N ≫ ncores cold requests on a 2-core profile → observed concurrency ≤ cap, p99 bounded, zero errors; warm gallery → zero PHP-FPM hits in the access log; A/B wall-clock vs PHP path.

---

## 11. Build order

| Milestone | Content | Exit gate |
|---|---|---|
| **M0 — storage foundation** | `nc-preview` crate skeleton, migration (§7), `snowflake.rs`, `store.rs` (queries, paths, insert/collision), `registry.rs` | Unit tests green; manual row insert readable by PHP (`occ preview:*` / direct query) |
| **M1 — serve hits + fallback** | `handlers.rs` for all three routes, authz, `size.rs`, header/304 parity, PHP-FPM proxy on any miss | Warm gallery: zero PHP requests, byte/header-identical to PHP, authz matrix verified |
| **M2 — native generation** | `backend.rs` (Imaginary), max-first flow, persist rows + bytes, `has-preview` wired to `registry.rs` (§11.1) | Cold JPEG/PNG/WebP/HEIC generated; golden pipeline diff clean; PHP serves the Rust row |
| **M3 — system behaviour** | `coalesce.rs` + semaphore, `invalidate.rs` on the PUT path | Load test passes; dedup = 1 backend call; overwrite invalidates both directions |
| **M4 — stretch** | Background max-preview pre-generation on upload, shutdown-drain aware | First gallery view predominantly hits |

M1 alone delivers the primary win (hits) with zero generation risk — it can ship and soak before M2.

---

## 12. Boot-time decisions

- Object-store instance (`objectstore` config present) → register the three routes as pure PHP-FPM proxies; skip the native path entirely (Phase 10 local-disk assumption).
- Imaginary misconfigured/unreachable at boot → native generation off, hit-serving **stays on** (rows PHP generated are still servable), misses proxy. Re-check on a TTL, not per request.

---

## 13. Deviations from PHP (explicit, documented per CLAUDE.md)

| Deviation | Kind | Consequence |
|---|---|---|
| Request coalescing (PHP has none) | Improvement | Fewer backend calls under burst; identical responses |
| ALL semaphore not replicated | Justified | Hits are cheap in Rust; NEW cap preserved |
| Generation survives client disconnect (detached task) | Improvement | Warms cache during gallery scroll-cancel; semaphore-bounded |
| `BeforePreviewFetchedEvent` not dispatched | Log-only loss | `admin_audit` gets no preview-fetch lines (its only listener) |
| On-demand legacy-layout migration not performed | PHP's concern | Rust generates fresh rows; possible duplicate storage until PHP migrates |
| GD-vs-Imaginary output-mime differences across writers | Accepted | Rows self-consistent per writer (match keys on max-row mime); worst case duplicate rows |
| ffmpeg/libreoffice PATH-search fallback (if not implemented) | Under-report only | `has-preview` false where PHP would say true — degrades to generic icon, never wrong content |

---

## 14. Risks & open questions

1. **Snowflake collision space with PHP.** `serverid` is shared config (or `crc32(hostname)` — identical on the same host), and Rust runs with `is_cli=0`, the same bit as PHP-FPM. If PHP's 12-bit sequence is shared across workers (`FileSequence` vs `APCuSequence` — **verify the binding in `lib/private/Server.php` before M2**), Rust is one more generator in the same `(ms, serverid, seq)` namespace. Correctness is preserved by the unique-retry on insert; if collision rate proves measurable, options: participate in PHP's `FileSequence` under `flock`, or document an ops recommendation to set a distinct `serverid` per participant.
2. **Legacy migration race:** if PHP's `on_demand_preview_migration` migrates flat-layout bytes for a file Rust already generated rows for, PHP's migration insert could hit the unique index. Verify PHP's migration handles the conflict; if not, hits-before-migration ordering makes it rare but not impossible — watch in soak.
3. **SVG/resource exhaustion** is bounded only by the source size cap and Imaginary's own limits — acceptable (same exposure as PHP).
4. **Imaginary 120 s timeout** vs impatient gallery clients: clients give up first; the detached generation still completes and warms the cache.
5. **Binary PATH search** for ffmpeg/LibreOffice (PHP `binaryFinder` fallback) — decide implement vs document (§13).

---

## 15. Alternatives considered

### 15.1 Image-processing strategy

| Alternative | Verdict | Why |
|---|---|---|
| **In-process libvips via FFI** | Rejected | Image decoders are the most CVE-dense surface in the stack (e.g. `CVE-2023-4863` libwebp). Rust is one long-lived process — one crafted upload would crash or corrupt the whole server, every connection at once. PHP's process-per-request model gets isolation for free; we must engineer it. Revisit only with hard sandboxing (seccomp/landlock per-worker or a WASM sandbox). |
| **`image` crate as generator** | Rejected | Slower and more memory-hungry than libvips; output not pixel-identical to PHP's libvips path (interop drift on byte-level comparisons); and it still pulls decoder CVE surface into the process. Matching libvips means binding libvips — which loops back to the FFI option. |
| **`vipsthumbnail` subprocess pool** | Kept as future backend | Same isolation as Imaginary, no second service to run — attractive for minimal self-hosted installs. Costs: process spawn per miss (~ms, acceptable), no HTTP health/ops story, supervision code we'd own. The `PreviewBackend` trait keeps this a swap-in, not a rewrite. |
| **Imaginary (chosen)** | Chosen | Already a first-class Nextcloud provider (`preview_imaginary_url`/`_key` exist, documented, deployed in the wild), thin HTTP-over-libvips, proven isolation + operational maturity. Its value over raw libvips is exactly the isolation boundary — not speed. |

### 15.2 Serve-path architecture

| Alternative | Verdict | Why |
|---|---|---|
| **Reverse-proxy cache in front of PHP** (nginx `proxy_cache` on `/core/preview`) | Rejected (as the primary) | The zero-Rust stopgap. Caches hits but does nothing for miss coalescing or generation admission, adds a second cache layer with its own invalidation story, and still pays PHP bootstrap on every miss and every cold client. Worth documenting for deployments not running the Rust binary — not for this project. |
| **In-process byte cache of preview images** | Rejected | Preview files are small and already OS-page-cache-warm; caching bytes in the Rust heap duplicates memory against the <64 MB idle NFR (REQ §20) and adds an invalidation surface for zero gain over `sendfile`. |
| **In-process row-metadata cache** | Deferred | A small TTL'd LRU of `file_id → rows` would remove the SELECT from the hottest tiles — but mixed Rust/PHP writes cap the safe TTL low. Tracked as [`improvements.md`](../../02-specifications/improvements.md) §I.10. |
| **Cross-process coalescing (Redis/memcached)** | Not needed | Rust is the sole listener (REQ §10.1) — one process, in-process coalescing suffices. Revisit only if multi-binary deployments behind an LB become a thing. |

### 15.3 Generation model

| Alternative | Verdict | Why |
|---|---|---|
| **Generate every size from the original** | Rejected | N full decodes of a 50 MB HEIC/RAW per gallery page — precisely the CPU thrash this phase exists to kill. PHP's max-first model (decode once, resize many) is the core insight; replicate it. |
| **Lazy max — decode at requested size only** | Rejected | Diverges from PHP's row set: PHP looks up the max and derives from it, so Rust-written odd-size rows wouldn't be found by PHP (and vice versa) — duplicate work and duplicate rows during coexistence. Interop requires the same rows, which means the same model. |
| **Async generation: 404 now, generate in background** | Rejected for interactive misses | The web Files app does not reliably re-request failed thumbnails — the user sees a broken tile until a manual refresh. Synchronous-with-coalescing matches PHP's wait semantics. Background generation stays in scope as **pre-generation on upload** (M4), where async is the whole point. |

### 15.4 Invalidation strategy

| Alternative | Verdict | Why |
|---|---|---|
| **Lazy: compare file etag/mtime at read** | Rejected | PHP never compares — it deletes on write. Read-side checks would add a filecache join to every hit (extra round-trip on the hottest path, against principle 2) and still diverge from PHP's observable behaviour. |
| **Synchronous deletion on the PUT critical path** | Rejected | A slow appdata unlink must not delay the write response. PHP can inline it (its requests are short-lived and isolated); the async server should not. Fire-and-forget + `warn!` + PHP's hourly orphan sweep as backstop. Accepted cost: a millisecond-scale window where a stale row can be served between PUT completion and the invalidation task — documented in §13. |

### 15.5 Id allocation

| Alternative | Verdict | Why |
|---|---|---|
| **DB sequence / `INSERT … DEFAULT`** | Impossible | Autoincrement was removed from all three tables (`Version33000Date20251023110529`) — no sequence exists on Postgres. |
| **Partition a private id range** | Rejected | Snowflakes are time-based; partitioning requires coordinating epoch/sequence bits PHP knows nothing about. Faithful replication of `SnowflakeGenerator` + unique-retry on insert is the only scheme under which Rust- and PHP-written ids provably coexist (collision analysis: §14 R1). |

---

## 16. Suggestions for improvements

Deliberately beyond PHP parity — none required for interop; each is opt-in or deferrable. The deferrable ones are registered in [`../../02-specifications/improvements.md`](../../02-specifications/improvements.md) so they survive this phase.

1. **Stale-while-revalidate on overwrite** (§I.11). Instead of delete-then-cold-miss, serve the stale tile immediately and regenerate in the background. Photo-editing workflows (overwrite-heavy folders) would never see a spinner. Deviation from PHP (which regenerates synchronously), so gate behind an opt-in config key.
2. **TTL'd row-metadata LRU** (§I.10). Removes the `oc_previews` SELECT from hot tiles; safe only with a short TTL (PHP writes can't notify us) and an `invalidate()` hook on Rust writes. Measure the hit path first — the SELECT is an indexed point lookup and may not be worth the surface.
3. **`Accept`-header format negotiation** (§I.12). Serve WebP/AVIF when the client signals support and such a row exists, independent of `preview_format`. Interop caveat: output mimetype is part of the match key, so negotiated formats create Rust-only rows PHP never looks for — harmless duplication, but keep it off by default.
4. **Optional SysV semaphore sharing** (§I.13). For deployments where Rust and PHP-FPM co-generate under a tight CPU budget, a config flag to have Rust also acquire `SEMAPHORE_ID_NEW` would enforce a true global cap instead of Rust-cap + PHP-cap. Only pays off while the PHP fallback path still generates.
5. **Bounded semaphore wait → fallback.** PHP blocks on `sem_acquire`; a tokio semaphore queues without bound. Under a pathological burst, prefer a bounded wait (e.g. 2 s) that falls back to the PHP proxy over unbounded queue memory. Cheap to add in M3.
6. **Batched pre-generation API.** PHP's `generatePreviews` accepts multiple specifications in one call. Expose the same internally for M4: max + the common web tiles (256, 512) in one coalescing session per upload.
7. **Imaginary connection hygiene.** Keep-alive pool with preconnect on the first miss of a burst; HTTP/2 if the Imaginary build supports it — trims per-miss latency by a round-trip.
8. **Bulk warm-up CLI.** A `nc-server preview-warm <user>` subcommand mirroring `occ preview:generate` for operators migrating large libraries — reuses the whole M2/M3 pipeline off the request path.

---

Prev: [`13-future-considerations-architectural-evolution.md`](13-future-considerations-architectural-evolution.md) · Up: [`README.md`](README.md)
