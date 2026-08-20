# 04 — iOS Photos tab places uploads without `PHAsset.modificationDate` on the upload day

**Status:** diagnosed + fixed 2026-08-21 (`media_mtime_ctime_fallback` on
`working`; commit pending). **Related:** [improvements.md D.1](../02-specifications/improvements.md)
(the deviation record), [phase-16.4 plan](../04-tasks/phase-16.4-put-parity-plan.md)
(X-OC-MTime/CTime parity), `nc-dav/src/mtime.rs`, `nc-dav/src/davfile.rs`,
`nc-dav/src/upload_handler.rs`. Client sources cited from
`~/Git/nextcloud-ios` (iOS client, tag 7.4.3 of NextcloudKit).

### What happened

WhatsApp-saved photos, auto-uploaded from the iOS camera roll, land in the
**correct `YYYY/MM` folder** but appear in the iOS app's **Photos tab (Media)
on the day of upload** — months later than the day they were saved. The
correct date is in flight: it arrives at the server in `X-OC-CTime` and is
stored in `oc_filecache_extended.creation_time`. Nothing reads it.

### What we found out

1. **The client sends two dates on every upload** (`NextcloudKit+Upload.swift:60-66`,
   background session `NextcloudKitBackground.swift:194-199`, chunked MOVE
   `NextcloudKit+Upload.swift:401-406`):
   - `X-OC-CTime` ← `metadata.creationDate` = `PHAsset.creationDate`
     (`NCCameraRoll.swift:236`), the Photos-framework timestamp — for
     third-party saves, the moment the app wrote the image to the library.
   - `X-OC-MTime` ← `metadata.date` = `PHAsset.modificationDate ?? Date()`
     (`NCCameraRoll.swift:237`). **WhatsApp saves set no modificationDate** —
     no EXIF (stripped) and the Photos framework records none — so the
     fallback stamps the **upload instant**. Both values are decimal unix
     seconds (`"\(timeIntervalSince1970)"`), e.g. `1726391102.123456`.
2. **The folder and the timeline use different dates.** Auto-upload computes
   the `YYYY/MM` path from `asset.creationDate`
   (`NCUtilityFileSystem.swift:909` — correct ✓). The Media tab issues a
   paginated WebDAV SEARCH with `WHERE`/`ORDER BY d:getlastmodified`
   (`NCMediaNetwork.swift:26,155-181`; `NCGlobal.swift:86` —
   `mediaPropOrder = "getlastmodified"`), i.e. it sorts by
   **`oc_filecache.mtime`** = what `X-OC-MTime` dictated. `creation_time` is
   never queried by the tab. (The tab is not EXIF-driven either way — the
   app's EXIF reader (`NCUtility+Exif.swift`) parses only already-downloaded
   files for the viewer info panel; server-side EXIF props
   (`nc:metadata-photos-gps`) come from the Photos app, which is not
   installed on this instance.)
3. **Server semantics (PHP, which the rewrite matched):** `mtime` ←
   `X-OC-MTime` (else request time), `creation_time` ← `X-OC-CTime` (else 0)
   — `File.php:346-366` (simple PUT), `ChunkingV2Plugin.php:195-203`
   (assembly MOVE). PHP never cross-derives the two, so the WhatsApp case
   was faithfully reproduced. Sanitizer asymmetry found while tracing:
   `MtimeSanitizer` (simple PUT) rejects `<= 86400`; `ChunkingV2Plugin`'s
   own sanitizer only requires `is_numeric` (no lower bound) — Rust already
   replicates both.
4. **The 120-second wall-clock idea fails on chunked uploads.** The
   `?? Date()` fallback is stamped at extraction time on the client; the
   assembly MOVE carrying `X-OC-MTime` arrives only after **all** chunks are
   uploaded — minutes to hours later for large videos. A window measured
   against the MOVE's arrival misses the fallback entirely. The robust
   anchor is server-side: iOS chunk PUTs carry no `X-OC-MTime`, so the chunk
   files' disk mtimes are server-observed arrival times, and the **earliest
   chunk's** mtime ≈ extraction time.
5. **A simple-PUT parity gap noticed en route (pre-existing, not fixed):**
   PHP's `View::touch()` sets the file's **disk** mtime from `X-OC-MTime` on
   simple PUT; the Rust simple-PUT path (`davfile.rs` flush) never touches
   the disk — only the cache columns. The fallback widens the cache/disk
   gap for the affected files. Follow-up.

### The options

- **Do nothing (PHP parity):** correct per spec, but the observable client
  bug persists — the "correct" data is stored where no consumer reads it.
- **Wall-clock epsilon on the MOVE** (`mtime >= now - 120s`): catches
  small files, misses slow chunked uploads by construction (finding 4).
- **Always `min(mtime, ctime)` for media:** re-anchors *edited* photos to
  their capture day — more opinionated than the user wanted, diverges from
  PHP for legitimately-modified media and desktop-synced photos.
- **Fallback-only, chunk-anchored window (chosen):** fires only when the
  sent `X-OC-MTime` is indistinguishable from the client's now-fallback.

### The decision (user-approved semantics + scope)

For media uploads (`image/*`, `video/*`) where **all** hold, the effective
mtime becomes `X-OC-CTime` (written to `mtime`/`storage_mtime`, propagation,
versions — every consumer of the original mtime):

- the `media_mtime_ctime_fallback` config key is enabled (default `true`);
- the client sent **both** headers (headerless → PHP semantics);
- sent `X-OC-MTime >= anchor − 15 min` — the fallback signature (15 min
  absorbs client↔server clock skew + local chunk splitting);
- `X-OC-CTime < sent mtime` (the mtime never moves forward).

**Anchor:** request open time for a simple PUT; the earliest chunk file's
disk mtime for a chunked MOVE. Scope: `davfile.rs` (PUT flush) +
`upload_handler.rs` (MOVE) only — `bulk_handler.rs` (desktop-only) stays at
strict PHP parity. Trade-off accepted and documented: media genuinely
modified within 15 minutes of arrival is re-anchored too — for fresh media
ctime ≈ mtime, and for edited media the tab then shows the capture day.

### Verification

- 14 unit tests for `media_mtime_fallback` in `nc-dav/src/mtime.rs` (window
  edge, non-media, switch off, missing headers, ctime ≥ mtime, no-chunk
  anchor, truncation already handled by the sanitizer).
- Workspace `cargo test --lib`: 571 green (331 in nc-dav), 0 failed.
- **Not difftest-coverable**: the oracle is PHP and cannot be told to
  diverge; the resolver is unit-tested instead. A live A/B against the dev
  stack is pending a `make sut-image` rebuild (the running container still
  has the old binary).

### Follow-ups

- Rebuild + live A/B: PUT an image with `X-OC-MTime` = now and an old
  `X-OC-CTime` against `:8080` (SUT) and `:9090` (PHP); confirm the SUT's
  `oc_filecache.mtime` = ctime, the PHP side keeps the header value, and
  `X-OC-MTime: accepted` is still returned on both.
- The simple-PUT disk-mtime gap (finding 5) — decide whether Rust should
  `set_file_times` from the effective mtime like PHP's `View::touch()`.
- Edge: a >15-minute client-side split of a giant video before the first
  chunk lands would miss the window — acceptable, but worth re-checking if
  the live A/B shows it.
