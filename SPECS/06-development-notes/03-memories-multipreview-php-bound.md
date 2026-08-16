# 03 — Memories multipreview — the gallery hot path — still round-trips PHP-FPM

**Status:** studied 2026-08-16 (memories shallow clone at `~/Git/memories`);
native handler **deferred by decision** — see [Decision](#the-decision). **Related:**
[note 01](01-static-assets-outside-apps.md) (memories `/wapps` static serving),
[Phase 11](../04-tasks/phase-11.md) (native preview fast path),
[plan 14](../03-implementation-plan/plan/14-native-preview-thumbnail-fast-path.md),
[plan 21](../03-implementation-plan/plan/21-propfind-round-trip-reduction.md),
[plan 22](../03-implementation-plan/plan/22-deployment-profile-tuning.md).

### What happened

The memories timeline (the primary gallery UI on the live instance) loads
thumbnails via

```
POST /index.php/apps/memories/api/image/multipreview
```

— a JSON body of ~30-40 `{reqid, fileid, x, y, a}` entries (~1.2 KB per
capture).  The route falls under Rust's `/index.php/{*path}` →
`php_fpm_fallback` (`router.rs:454`), so every timeline batch pays a PHP
boot + app load + an N+1 query pattern + strictly serial per-file work —
while the Phase 11 native preview fast path (`nc-preview`) covers only
`/core/preview` (`router.rs:387-390`), which the memories frontend never
calls.

### What we found out (the endpoint, source-grounded)

1. **Protocol** (`memories/lib/Controller/ImageController.php:98`): filter
   entries with missing/zero params (`:102`), sort by area ascending —
   smallest thumbnails stream first so the grid renders progressively
   (`:110`) — then stream an `application/octet-stream` of per-file frames:
   `[1 byte: json length][json {reqid, len, type}][image bytes]` (`:164-168`).
2. **One batch query**: `getAvailablePreviews(fileIds)` — `oc_previews WHERE
   file_id IN (...)` + location join
   (`workspace/server/lib/private/Preview/Db/PreviewMapper.php:112-123`).
   The batch **capability** is core — a service any app can call
   (`PreviewService.php:136` → `PreviewMapper.php:112`); the **HTTP
   endpoint and wire protocol** are memories-only — core's HTTP surface is
   single-preview (`/core/preview`), no batch endpoint exists.
3. **Max gate**: a file is served only if it already has an `isMax()` preview
   row (`ImageController.php:137`) — generation happens in the background
   `previewgenerator` job, not here; gated-out files are skipped silently.
4. **Per surviving file, serial**: `getUserFile` (filecache lookup,
   `:148`) → `Generator::getPreview(x, y)` (`Generator.php:59`) which
   **re-queries `oc_previews` for the single file**
   (`Generator.php:113` — the controller's batch result is discarded: an
   N+1) → `getMaxPreview` selects the `isMax` row or generates the max
   (`Generator.php:323-360`) → exact-size cache hit reads the file, miss
   scales/generates via imaginary/imagick/gd.  Nothing is parallelized.
5. **Client behaviour** (`memories/src/components/frame/XImgWorker.ts`):
   single pending images skip multipreview (`:91-93`); served images are
   cached client-side for **7 days** (`cache-control: max-age=604800`,
   workbox cache `memories-images`, `:25,113`).  Each thumbnail hits the
   server ~once per 7 days per browser — the endpoint is a **cold-cache
   path**, not a per-scroll hot path.

### The decision

**Do not implement a native handler now.** Reasons:

- **Brittleness of a third-party contract.** The framing is consumed by
  strict client arithmetic (`XImgWorker.ts` buffer math) — a format change
  upstream would corrupt images silently in the browser, not fail honestly.
  `a` is a stringified bool (`ImageController.php:154`); the `isMax()` gate
  depends on config (`preview_max_x/y`, ratio, mode, version); the
  URL/request shape is unversioned and memories ships fast.  A native
  replication hardcodes an app-specific surface into the Rust router.
- **The profile doesn't pay for it** (plan 22 verdict): round trips are the
  abundant resource (~50 µs localhost unix socket) — the N+1 and the batch
  form are the wrong currency here; CPU is scarce but the endpoint's PHP
  cost is bounded and amortized over the 7-day client cache.
- **The expensive CPU is not in this endpoint.** The max gate means it never
  generates on demand; decode + resize lives in the background
  `previewgenerator` — plan 22 **P7** (1 concurrent job, `nice`/`ionice`) is
  the governance target that actually matters.

**Tripwire:** a Wave-0 measurement of multipreview's PHP CPU on the target
profile (per-call + under concurrent gallery load).  If it measurably
competes with request handling, the candidates are the native endpoint
replication (the gated design below — protocol-brittleness risk) or Seam A
core delegation (below — core-patch + interop-surface risk); the
measurement decides which.

**Seams — where delegation can happen.**  A PHP app cannot call Rust
in-process, so the seam decides the brittleness profile:

- **Seam A — PHP-core delegation (zero app changes):** patch core
  `PreviewService::getAvailablePreviews` / `Generator::getPreview` to
  delegate the batch lookup + cached-file serving to a Rust internal
  endpoint (loopback HTTP + trust token; the `__session_resolve` shim is
  the existing interop precedent, in the reverse direction), PHP falling
  back to its own implementation on any error.  Every core-preview
  consumer benefits untouched.  Costs: patches the PHP core being replaced,
  and a new interop surface — trust model, fallback semantics, and
  loop-avoidance (the internal endpoint must be served natively, never
  proxied back to PHP).
- **Seam B — core-owned HTTP batch endpoint + one-time app patch:**
  **Rejected 2026-08-16** — upstream memories maintainers will not support
  a patch redirecting their frontend to a core-owned endpoint, and forking
  the app we run diverges from upstream.  Not an option.

### The design (gated reference — if the tripwire fires)

**The win, reframed for the target profile** (plan 22): CPU (PHP boot +
per-file machinery on a 2-core shared budget) and I/O discipline (HDD
seeks), not round trips.  Decisions grounded in the studies:

| decision | why (plan ref) |
|---|---|
| **1 concurrent generation job**, `nice`/`ionice`'d — misses via the existing imaginary backend with the profile cap | P7 — parallel imaginary on 2 cores starves requests |
| reads bounded by the **P4** I/O semaphore (~2-4 permits) + blocking-thread cap; `fadvise` per **P5** (WILLNEED before the DB query, SEQUENTIAL on stream, DONTNEED after) | HDD elevator/NCQ — more concurrency *lowers* throughput |
| plain batch `IN` / `= ANY($1::bigint[])` **stable statement text**, SQLite `IN` behind the dialect check; **no CTE / `json_agg`** | T4 stable-text form (plan 21); P0/Wave-0 lesson (plan 22) — JSON serialization CPU both sides, net-negative here |
| the **max-gate skip** retained | do-less-work (plan 21 T6 spirit; plan 22 consequence #2) |
| a **perf-budget gate entry** `multipreview`, PHP baseline measured first | plan 21's guardrail (§20) |

**Sequencing** (plan 22 wave style): Wave 0 — baseline (statement count via
the PG-log perf-gate method + latency, `:8080` vs `:9090`); Wave 1 —
`load_preview_rows_batch` (`nc-preview/src/store.rs`, today per-file at
`store.rs:148`), route + handler (framing, area-sort, max-gate, 2 batch
queries, governed reads, imaginary at cap 1), `perf-budget.yaml` entry;
Wave 2 — perf-gate, latency bench, byte-parity replay scenario (achievable:
imaginary shared, `nc-preview/format.rs` documents byte-identical
`operations` JSON).

### Wave 0 — baseline measured (dev stack, 2026-08-16)

Method: memories 8.1.0 installed into the dev container, one seeded photo
(max preview generated), PG statement logging to the container's
`/var/log/postgresql/*.log` (`logging_collector=on`, `log_statement=all` —
the perf-gate technique; `docker logs` delivery is unreliable), per-request
windows extracted by timestamp.

**2-entry call** (one cached 64×64, one 128×128) — **17 statements**:

- fixed ~10-12: session/auth (oc_users, oc_authtoken, twofactor, group,
  appconfig, preferences, storages, mimetypes) + filesystem setup
  (oc_mounts ×2, filecache path lookups ×2)
- the N+1, confirmed: **3 oc_previews for 2 files** — the controller's one
  batch `IN` query, then `Generator::getPreview` re-queries per file
  (`Generator.php:113`)
- per-file marginal: 1 oc_previews re-query + 1 filecache `getUserFile` +
  generator work (imaginary HTTP round trip on miss — invisible in PG)

Latency (warm, dev): **~50-90 ms** — PHP boot dominates; the Rust proxy hop
(`:8080`) adds ~2-10 ms over direct PHP (`:9090`).

Extrapolation (arithmetic, structural — the loop is a serial `foreach`):
a real ~30-file call ≈ 10 fixed + 1 batch + 30×2 ≈ **~70-80 statements**.

Caveat: statement counts are profile-independent, but the tripwire is CPU
on the **target** profile — the dev numbers close the statement ledger, not
the CPU ledger; the target-profile CPU measurement is still outstanding.
