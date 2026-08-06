# Plan — close the 16.4 PUT divergence (native single-file PUT ↔ PHP)

> Status: **done** (2026-08-05, code in commits `7b6b501`–`ee6e38b`; documented 2026-08-06).
> Companion to [`phase-16.md`](phase-16.md) §16.4 deviation.
> Ground truth: PHP source in `workspace/server/` (commit `1a0ccac9…`) + requirements
> [`../01-requirements/requirements/06-webdav-dav.md`](../01-requirements/requirements/06-webdav-dav.md) §6.8 and
> [`../01-requirements/requirements/21-filecache-population.md`](../01-requirements/requirements/21-filecache-population.md) §21.
> This is a **plan**, not a spec: it records root causes and intended Rust changes. On completion the
> outcome is logged back into `phase-16.md` (a note *below* 16.4 + the `## Changes` log — the 16.4 task body
> itself stays verbatim per the documentation conventions).

## Context — why

Phase 16's differential harness is built and proven (16.1–16.9), but the write scenarios are **correctly
red**: the current Rust build is not behaviorally identical to PHP on a bare `PUT`. The 16.4 deviation note
records five live-verified Rust↔PHP divergences from `10_put_get` (fresh file). This is exactly the
"silent DB side-effect" class the rewrite exists to eliminate (CLAUDE.md principles 1/3/4, hygiene rules
3/4/7): the HTTP response is already perfect (PUT answers `201`/`204` on both sides), but Rust skips or
mis-shapes downstream writes PHP performs.

Goal: make the native single-file PUT path — `crates/nc-dav/src/davfile.rs::flush()` — reproduce PHP's
write-side effects exactly, so `10_put_get` / `10_put_get_delete` move toward `IDENTICAL`.

**Session constraint:** the dev docker (`master-*`) is in use elsewhere, so no live probing / diff runs in
this session. Land code + SQLite-backed unit tests + `cargo build` / `cargo test --lib` now; defer the A/B
diff verification to when the docker is free (steps at the bottom).

## PHP mechanics (the target), traced

Single-file DAV PUT is `apps/dav/lib/Connector/Sabre/File.php::put()`:

1. Content → part file, assembled with `$storage->moveFromStorage(...)` (storage level, no `View`).
2. `$storage->getUpdater()->update($internalPath)` — `File.php:337`. This is the cache side-effect core:
   `lib/private/Files/Cache/Updater.php:68-94`, in order:
   - shallow `scan` rebuilds the file's own `oc_filecache` row (mtime/storage_mtime/size/etag); returns
     `oldSize`/`size` (`Scanner.php:216` sets `oldSize` only when a row already existed).
   - `sizeDifference = size − oldSize` when both present; **null for a brand-new file**.
   - if `sizeDifference === null` → `Cache::correctFolderSize($path)` (`Cache.php:956-977`) — **recomputes**
     each ancestor's size from the sum of its children, walking up to the storage root.
   - `correctParentStorageMtime($path)` (`Updater.php:225-241`) — sets the **direct parent's**
     `storage_mtime` *and* `mtime` (via the `storage_mtime→mtime` copy in `Cache::normalizeData`,
     `Cache.php:468-471`) to the parent **directory's disk mtime**.
   - `propagateChange($path, $time, $sizeDifference ?? 0)` (`Propagator.php:42-151`) — one `UPDATE` over all
     ancestors (`getParents`, `Propagator.php:156-165` = `['', 'files', …]`): `etag` = one shared value,
     `mtime = GREATEST(mtime, time)`, `size = GREATEST(size + diff, -1)` **only where `size > -1`**. For a
     new file the diff is `0` (size already handled by `correctFolderSize`) — etag/mtime only.
3. `X-OC-MTime` → `touch` (`File.php:346-352`); then `putFileInfo(['upload_time' => time()]` + `creation_time`
   **only** when `X-OC-CTime` was sent) (`File.php:354-366`). `Cache::normalizeData` (`Cache.php:446-487`)
   drops falsy extension fields, so a bare PUT writes only `upload_time = <request time>` and leaves
   `creation_time = 0` (the column default) — this is the resolved finding #3.
4. `emitPostHooks` → `post_write` → `NodeWrittenEvent` cascade (`HookConnector.php:77-84`):
   - `files_versions` `FileEventsListener` → `createVersionEntity` / `updateVersionEntity`
     (`LegacyVersionsBackend.php:232-279`), then `VersionAuthorListener` sets `metadata['author']=uid`
     → the column is `Types::JSON` → PHP `json_encode` → **compact** `{"author":"admin"}`.
   - `previewgenerator` `PostWriteListener` (`PostWriteListener.php:33-70`) → `INSERT (uid, file_id,
     queued_at)` into `oc_preview_generation` **only if no row for that `(uid, file_id)` yet**.

## The five divergences → root cause → fix

| # | Divergence (16.4) | Rust today | Root cause | Fix |
|---|---|---|---|---|
| 3 | `oc_filecache_extended` creation/upload time | `creation_time = x_oc_ctime.unwrap_or(now)`, `upload_time = use_mtime` | over-populates `creation_time`; uses file mtime, not request time, for `upload_time` | `creation_time = x_oc_ctime.unwrap_or(0)`; `upload_time = now` |
| 4 | `oc_files_versions.metadata` spacing | `format!("{{\"author\": \"{uid}\"}}")` → space after colon | hand-rolled JSON | compact `serde_json` → `{"author":"admin"}` |
| 5 | `oc_preview_generation` row | nothing | PUT fires no write-event; no queue insert anywhere | add insert-if-absent in the PUT commit path |
| 2 | `files/` `storage_mtime` bump | propagator sets only `mtime`/`etag`/`size` | no `correctParentStorageMtime` equivalent | add it (parent `storage_mtime`+`mtime` ← parent dir disk mtime) |
| 1 | storage-root size over-propagation | always `propagate_change(path, mtime, size−old_size)` — a signed delta even for a new file | doesn't split new-file (recompute) vs overwrite (delta) | mirror `Updater::update`: new file → `correctFolderSize` + `propagateChange(0)`; overwrite → `propagateChange(size−old)` |

Findings 1 & 2 share the propagation block at the end of `flush()`; the fix must reproduce PHP's operation
order: `[correctFolderSize if new]` → `correctParentStorageMtime` → `propagateChange`.
`Propagator::propagate_change` already matches PHP's `propagateChange` (etag/mtime/size + `size > -1` guard)
and **stays**; what's wrong is how/with-what-arguments `flush()` calls it, plus the two missing helpers.

## Changes (all in `core-rs/crates/nc-dav/` unless noted)

### `src/davfile.rs` — the commit path (findings 1, 2, 3, 5)

Inside `flush()`:

- **Finding 3.** Line ~367: `let use_creation_time = ctx.x_oc_ctime.unwrap_or(now);` →
  `ctx.x_oc_ctime.unwrap_or(0)`. Line ~519: bind `upload_time = now` (request time) instead of `use_mtime`.
  The `ON CONFLICT DO UPDATE SET upload_time = excluded.upload_time` clause already preserves
  `creation_time` on overwrite (PHP updates only `upload_time` there) — keep it. Leave the `oc_filecache`
  `mtime`/`storage_mtime` bindings on `use_mtime` (already PHP-correct).
- **Findings 1 & 2.** Replace the single `propagate_change(fc_path, use_mtime, size_diff)` call (lines
  ~526-536) with the PHP `Updater::update` sequence; `is_new = ctx.initial_fileid.is_none()`:
  - `is_new` → `propagator.correct_folder_size_chain(&fc_path)` then
    `propagator.correct_parent_storage_mtime(&parent_fc_path, &parent_disk_path)` then
    `propagator.propagate_change(&fc_path, use_mtime, 0)`.
  - overwrite → `correct_parent_storage_mtime(...)` then `propagator.propagate_change(&fc_path, use_mtime,
    size_diff)` where `size_diff = size − ctx.old_size` (existing behavior).
- **Finding 5.** After `insert_version_entity` (end of `flush()`), call a new
  `crate::preview_queue::queue_preview_generation(pool, prefix, &ctx.uid, fileid, now)`. New tiny module
  `src/preview_queue.rs` + `mod` in `lib.rs`.

### `src/propagator.rs` — two new helpers (findings 1, 2)

- `correct_parent_storage_mtime(&self, parent_fc_path: &str, parent_disk_path: &Path)`:
  read the parent directory's disk mtime (`std::fs::metadata`, truncated to seconds), then
  `UPDATE {p}filecache SET storage_mtime=$t, mtime=$t WHERE storage=$s AND path_hash=md5(parent)`. Setting
  both mirrors PHP's `storage_mtime→mtime` copy (`Cache.php:468-471`). The subsequent `propagate_change`
  applies `GREATEST(mtime, time)`, matching PHP ordering.
- `correct_folder_size_chain(&self, fc_path: &str)`: mirror `Cache::correctFolderSize` (`Cache.php:956-977`) —
  recompute each ancestor's size from the sum of its direct children, walking from the file's parent up to
  and including the storage root, updating only where the value changes; leave `size = -1` (unscanned) rows
  as `-1`. The existing single-folder `correct_folder_size` stays for MOVE/COPY callers.

### `src/versions.rs` — finding 4

- Line ~492: replace the `format!` metadata with a compact encoding matching PHP `json_encode`, e.g. extract
  `fn version_metadata_json(uid: &str) -> String { serde_json::json!({ "author": uid }).to_string() }`
  (`serde_json` already a dependency) → `{"author":"admin"}`, with correct escaping for unusual uids.

### `src/preview_queue.rs` (new) — finding 5

- `queue_preview_generation(pool, prefix, uid, file_id, queued_at)`:
  `INSERT INTO {p}preview_generation (uid, file_id, queued_at) SELECT $1,$2,$3 WHERE NOT EXISTS (SELECT 1
  FROM {p}preview_generation WHERE uid=$1 AND file_id=$2)`. The table has **no** unique constraint on
  `(uid, file_id)` (migrations only add PK `id`, then `queued_at`), so use the existence guard, not
  `ON CONFLICT`. Log-and-continue on error (hygiene rule 1).

### Deliberately not touched

- `bulk_handler.rs` is **green** today; leave it. (Open question below: PHP reading says bulk/`newFile`
  should also write `upload_time=0` and queue preview_generation, yet bulk is IDENTICAL — verify live before
  touching bulk.)
- `Propagator::propagate_change`, and the later-phase findings (#6–#27: trash/COPY/MKCOL/mtime details), are
  out of scope; finding 2's helper becomes reusable for the MKCOL/storage_mtime findings (#12/#17/#21/#22)
  in a follow-up.

## Unit tests (add now; SQLite-backed, no docker)

Follow the existing in-memory-SQLite pattern in `propagator.rs`:
- `propagator.rs`: seed the 4-row tree; assert `correct_parent_storage_mtime` sets parent
  `storage_mtime`+`mtime`; assert `correct_folder_size_chain` recomputes `files` and root sizes after
  inserting a new child, and leaves an unscanned `size=-1` ancestor untouched.
- `versions.rs`: assert `version_metadata_json("admin")` == `{"author":"admin"}` exactly (no space).
- `preview_queue.rs`: the guard inserts once and is a no-op on a second call for the same `(uid, file_id)`.

Then `cargo build -p nc-dav` and `cargo test --lib -p nc-dav` (plus workspace `cargo test --lib` to confirm
no regression). These run without the docker.

## Verification (deferred until the docker is free)

1. `docker compose up -d --build nextcloud` then `docker compose restart proxy` (CLAUDE.md rebuild rules).
2. `cargo run -p nc-difftest --bin difftest -- run crates/nc-difftest/scenarios/10_put_get.yaml` and
   `10_put_get_delete.yaml` — expect the five divergence blocks to disappear.
3. Re-run `16_overwrite_put.yaml`, `18_explicit_mtime.yaml` (they inherit these PUT side-effects) and
   `21_bulk_upload.yaml` (regression guard — must stay IDENTICAL).
4. Log the outcome in `phase-16.md`: a note **below** the 16.4 task (never editing the task body) + a
   `## Changes` entry; check off findings as the diffs confirm them.

## Open questions / risks (flagged, not blockers)

- **Finding 1 exact root-size semantics.** I could not fully pin down *why* PHP leaves the storage-root
  `size` unchanged while recomputing `files/` (both are ancestors in `getParents`). The fix mirrors PHP's
  `correctFolderSize` recompute for new files. If the live diff still shows a root-size gap, inspect the live
  `oc_filecache` root row (`docker exec master-database-pgsql-1 psql -U postgres -d nextcloud`) to determine
  whether PHP keeps the home-root `size` at `-1`/computed and adjust to match observed PHP.
- **Bulk contradiction.** PHP source reading says `newFile` (bulk) writes `creation_time=now, upload_time=0`
  (`Folder.php:163-186` + `View::file_put_contents` sets no upload_time) and queues preview_generation, yet
  `21_bulk_upload` is green while Rust bulk sets `upload_time=now` and never queues. This needs a live
  snapshot comparison before trusting either the bulk-green result or my reading of the Node-API event wiring.
  Do **not** change bulk until then.
- **preview_generation in core.** Preview queueing is app (`previewgenerator`) behavior in PHP; reproducing
  it in `nc-dav` is the parity-driven choice (the app is enabled on both instances). Alternative — adding the
  table to `SKIP_TABLES` — would mask a real behavioral difference and is not recommended.
