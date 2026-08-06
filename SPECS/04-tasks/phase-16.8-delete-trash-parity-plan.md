# Plan — close the delete-to-trash divergence (findings #6–#12)

> Status: **implemented** (2026-08-07) — fixes landed in `filesystem.rs`, unit-tested, and
> difftest-verified on a fresh stack; outcome logged in `phase-16.md` §Changes.
> Originally planned 2026-08-06; ground truth re-verified live (see §Ground truth).
> Companion to [`phase-16.md`](phase-16.md) §16.8 Changes.
> Ground truth: PHP source in `workspace/server/` (commit `1a0ccac9…`) **and** live DB experiments
> against the oracle instance.
> This is a **plan**, not a spec: it records root causes and intended Rust changes. On completion the
> outcome is logged back into `phase-16.md` (a note *below* the task + the `## Changes` log — the task
> body stays verbatim per the documentation conventions).

## Context — why

The 16.4 PUT parity plan (findings #1–#5) is complete. The differential harness's next-largest
divergence cluster is the **delete-to-trash flow** (findings #6–#12, recorded in phase-16.md §16.8
Changes). These share the same `filesystem.rs`/`propagator.rs` code paths, and the 16.4 helpers
(`correct_parent_storage_mtime`, `correct_folder_size_chain`, `queue_preview_generation`) are
directly reusable here.

Goal: make the native delete-to-trash path reproduce PHP's `Trashbin::move2trash()` write-side
effects exactly.

## Ground truth re-verified live (2026-08-06)

A controlled experiment against the live oracle (fresh PUT + DELETE of a probe file, DB inspected
immediately after each op) confirmed the following PHP behaviors — several correct the original
findings below:

1. **PUT creates an `oc_files_versions` row** for every written file: `file_id` = file id,
   `timestamp` = **file mtime** (not wall-clock), `size`/`mimetype` from the file, `metadata` initially
   `[]`, then `{"author": …}` via `VersionAuthorListener` (matches the SUT's
   `davfile.rs:618` insert).
2. **DELETE-to-trash deletes that row unconditionally** — the final state has NO version row, even
   when no version *file* exists on disk. The deletion happens inside the trash flow (Node events
   don't fire for the storage-level move; mechanism of the row removal is not the remove-hook chain —
   the observable is what matters: row gone).
3. **Trash ancestors get size updates** (`files_trashbin`/`files_trashbin/files` end with the
   trashed file's size) — and NO etag/mtime updates of their own: their final etag comes from the
   trash-chain propagation (below).
4. **The etag mechanism is a three-step sequence** (fully resolved — the earlier "target-chain
   propagation has no effect" hypothesis was wrong):
   - `renameFromStorage`'s source `propagateChange` stamps `[root, files]` with etag A;
   - `renameFromStorage`'s target `propagateChange` stamps `[root, files_trashbin,
     files_trashbin/files]` with etag B;
   - `View::unlink`'s post-op `Updater::remove` (View.php:1247-1248 → Updater.php:102-115) runs
     **after** the trash move and stamps `[root, files]` with etag C — the final root writer.
   Post-delete oracle state: `root.etag == files.etag` (C) and `files_trashbin.etag ==
   files_trashbin/files.etag` (B); `keys`/`versions` keep their mkdir/insert etags.
5. **`setUpTrash`'s mkdirs carry side effects** (`View::mkdir` → `basicOperation` →
   `Updater::update` → `correctParentStorageMtime(parent)` + `propagateChange(mkdir'd dir)`): the
   **storage root's `storage_mtime` becomes `now`** because `files_trashbin` is a direct child of
   the root; each mkdir'd dir gets `storage_mtime = mtime = now` from the insert/scan.
6. **Preview queue is NOT touched by the trash flow.** (pixel.png probe: PUT queues
   `oc_preview_generation` (id 3); DELETE leaves it in place, no new row.) The SUT's trash path
   already matches.
7. The trashed file's own row keeps `etag`/`mtime` (path/parent/name re-key only); its
   `storage_mtime` = disk mtime after the rename (= preserved PUT mtime).
8. **The delete flow materializes the user's `cache/` filecache row** (absent after PUT, present
   after DELETE; scanner-insert shape — size 0, permissions 31, no extended row). The triggering
   read is not identified in the PHP source; the observable is replicated. This is finding #8 —
   previously deferred, now **in scope**: its absence shifts the harness's etag sentinel
   numbering for every subsequent row, so the etag rows cannot clear without it.

All of the above was re-verified on the post-reset (`docker compose down -v && make diff-up`,
2026-08-06) stack: the fresh baseline run of `10_put_get_delete` shows exactly these oracle-side
rows and the SUT-side divergences (root etag == files_trashbin, surviving version row, stale root
storage_mtime, missing cache row).

## PHP mechanics (the target), traced

PHP `Trashbin::move2trash()` (`apps/files_trashbin/lib/Trashbin.php:232-373`):

1. **`setUpTrash($user)`** (line 257) — ensures these dirs exist via `View::mkdir()`:
   `files_trashbin`, `files_trashbin/files`, `files_trashbin/versions`, `files_trashbin/keys`.
   Each `mkdir` → `Storage::mkdir()` → `Cache::insert({size,mtime,mimetype})` — **no
   `oc_filecache_extended` row is created** (`normalizeData` separates extension fields only when
   present in the insert data; mkdir passes none).

2. **`$trashStorage->moveFromStorage(...)`** — storage-level rename.

3. **`$trashStorage->getUpdater()->renameFromStorage(...)`** (line 311) — the critical cache
   side-effect call. `Updater::copyOrRenameFromStorage()` (`Updater.php:158-205`) does, in order:
   - `$sourceCache->correctFolderSize($source)` — recompute SOURCE ancestor sizes (remove file's size)
   - `$this->cache->correctFolderSize($target)` — recompute TARGET ancestor sizes (add file's size to trash ancestors)
   - `$sourceUpdater->correctParentStorageMtime($source)` — set source parent's `storage_mtime` from disk mtime
   - `$this->correctParentStorageMtime($target)` — set target parent's `storage_mtime` from disk mtime
   - `$this->updateStorageMTimeOnly($target)` — update file's own `storage_mtime` (keep `mtime` as-is)
   - `$sourcePropagator->propagateChange($source, $time)` — etag/mtime propagation up source chain
   - `$this->propagator->propagateChange($target, $time)` — etag/mtime propagation up target chain

4. **`self::retainVersions($filename, $owner, $ownerPath, $timestamp)`** (line 355) — moves version
   files from `files_versions/` to `files_trashbin/versions/` so they can be restored; the
   `oc_files_versions` rows are effectively re-parented (the version file's filecache row gets a new
   path under `files_trashbin/versions/`).

5. PHP's `NodeWrittenEvent` cascade fires through `Updater::renameFromStorage` →
   `postRenameHook` → previewgenerator's `PostWriteListener` → inserts `oc_preview_generation` for
   the trashed file.

---

## Divergences → root cause → fix

### #6 — Trashbin folder sizes (missing target-side propagation)

**Root cause:** `delete_file` calls `propagate_change(fc_path, now, -deleted_size)` — subtracts from the
original parent chain only. It never adds the size to the trash parent chain. PHP's
`renameFromStorage` calls `correctFolderSize` on BOTH source AND target chains.

**Fix in `delete_file`:**
- Capture `trash_fc` from `self.move_to_trash(fc_path, &frow).await?`
- After the existing `propagate_change(fc_path, now, -deleted_size)`, add for the target chain:
  ```rust
  propagator.correct_folder_size_chain(&trash_fc).await;
  propagator.propagate_change(&trash_fc, now, 0).await;
  ```

**Fix in `delete_dir`:** Same pattern — capture trash_fc, propagate to both chains.

### #7 — Trashbin skeleton dirs (missing `keys`/`versions`)

**Root cause:** `move_to_trash` → `ensure_parent_dir(&trash_parent_fc)` creates `files_trashbin` +
`files_trashbin/files` for the path to the trashed file, but PHP's `setUpTrash()` also creates
`files_trashbin/keys` and `files_trashbin/versions`.

**Fix:** After `ensure_parent_dir(&trash_parent_fc)`, also ensure `files_trashbin/keys` and
`files_trashbin/versions` exist — either inline `ensure_parent_dir` calls or a small
`ensure_trash_skeleton()` helper.

### #8 — Lazy `cache/` materialization

**Skipped.** This is not delete-path-specific — PHP lazily materializes the user's `cache/`
filecache row on first files access. A broader concern for a separate fix.

### #9 — Extended rows on trash ancestor dirs

**Root cause:** `ensure_parent_dir` (filesystem.rs:277-294) unconditionally INSERTs an
`oc_filecache_extended` row with `creation_time = upload_time = now` for every directory it creates.
PHP's `View::mkdir` → `Cache::insert` does NOT create extended rows for directories — `normalizeData`
only separates extension fields (`creation_time`, `upload_time`, `metadata_etag`) when they're
explicitly present in the insert data, and mkdir passes none of them.

**Fix:** Remove the `oc_filecache_extended` INSERT block from `ensure_parent_dir` (lines 277-294).
PHP never creates extended rows for mkdir'd directories; Rust shouldn't either. This matches PHP
across ALL callers (PUT ancestors, trash ancestors, copy ancestors), not just the trash path.

**Safety:** `row::get_extended` returns `ExtendedRow { creation_time: 0, upload_time: 0,
metadata_etag: String::new() }` when no row exists — safe.

### #8 (rescoped) — Lazy `cache/` materialization on delete

**Root cause (verified):** the delete flow materializes the user's `cache/` filecache row
(scanner-insert shape; absent after PUT, present after DELETE). Deferred in the original plan as
"not delete-path-specific", but it appears IN the delete delta and shifts the harness's etag
sentinel numbering for every subsequent row — so the etag rows cannot clear without it.

**Fix:** in `move_to_trash`, create-if-missing the `cache/` row (mimetype
`httpd/unix-directory`, size 0, `mtime = storage_mtime = now`, permissions 31, no extended row,
parent = root).

### #10 — `oc_files_versions` rows survive the trash move

**Root cause (verified):** PHP creates a version row on every PUT (`created()` →
`createVersionEntity`, timestamp = file mtime, `metadata={"author":…}` — the SUT's `davfile.rs:618`
insert matches) and **deletes it during the trash flow unconditionally** — the View-level
`delete` hook bridges to `BeforeNodeDeletedEvent`/`NodeDeletedEvent` (`HookConnector`), the
versions `remove_hook` → `deleteVersionsEntity` → `deleteAllVersionsForFileId` (a plain
`DELETE … WHERE file_id = ?`), regardless of whether any version *file* exists. Rust's
`trash_versions` gates that DELETE behind the version-file query's early return, so the row the
SUT's own PUT inserted survives the delete.

**Fix (implemented):** the `oc_files_versions` DELETE runs **unconditionally** at the top of
`trash_versions`, matching by the trashed node's OWN file id (`file_id = $1`) — PHP parity for
directories too (the hook fires only for the deleted node, so a dir trash deletes nothing; inner
files' version rows survive with their unchanged fileids).

### #11 — Preview re-queue on trash — **NO FIX (verified non-divergence)**

**Root cause (verified — the original claim was wrong):** PHP's trash flow does **not** queue
`oc_preview_generation` and does **not** remove an existing queue row (pixel.png probe: PUT queues
id 3; DELETE leaves it). The SUT's trash path (no queue insert, row survives) already matches.
**Do not** add a queue call — that would introduce a divergence.

### #12 — Root `storage_mtime` + source-chain-last etag ordering

**Root cause (verified):** Two distinct effects, both currently missing/wrong in Rust:

1. **Root `storage_mtime`:** PHP's `setUpTrash` mkdirs run through `View::mkdir` →
   `Updater::update` → `correctParentStorageMtime(parent)` — so creating `files_trashbin` (a direct
   child of the storage root) sets the **root's** `storage_mtime = now`, and each mkdir'd dir's
   parent gets the same treatment. Rust's `ensure_parent_dir` inserts rows but has no such side
   effects → root `storage_mtime` stays stale.
2. **Etag ordering:** PHP stamps `[root, files]` (source), then `[root, files_trashbin,
   files_trashbin/files]` (trash chain), then — via `View::unlink`'s post-op `Updater::remove` —
   `[root, files]` **again**, as the final root writer. Rust's `delete_file`/`delete_dir` ran the
   source chain FIRST and the trash chain LAST, so the trash chain won on root → `root ≠ files`.

**Fix (implemented):**
- `ensure_parent_dir`: after creating each missing dir, mirror PHP's mkdir side effects —
  `correct_parent_storage_mtime(parent_fc, parent_disk)` on the new dir's parent, then
  `propagate_change(created_dir, now, 0)` (one shared etag for all ancestors). Generic
  `View::mkdir` parity (also correct for PUT/MKCOL ancestor mkdirs). The mkdir etag stamps are
  transient (later propagations overwrite them); the storage_mtime stamp is the observable fix.
- `delete_file`/`delete_dir`: **reorder** — `propagate_trash_target` (sizes, both parents'
  `storage_mtime`, trash-chain etag) runs FIRST, the source-chain `propagate_change` LAST —
  matching PHP's `Updater::remove` being the final root writer. Result: `root.etag == files.etag`
  and `files_trashbin.etag == files_trashbin/files.etag`, exactly the oracle's equality pattern.
  `propagate_trash_target` itself is unchanged.

---

## Files modified

| File | Change |
|---|---|
| `core-rs/crates/nc-dav/src/filesystem.rs` | `delete_file`/`delete_dir` (trash-chain-first ordering), `move_to_trash` (cache row materialization), `ensure_parent_dir` (mkdir side effects), `trash_versions` (unconditional by-id version-row delete) |

## Not touched

- `propagator.rs` — `correct_parent_storage_mtime`, `correct_folder_size_chain`, and
  `propagate_change` already exist from the 16.4 fix; reused as-is.
- `preview_queue.rs` — already exists from the 16.4 fix; reused as-is.
- `versions.rs` — version-file move stays inlined in `filesystem.rs::trash_versions`.
- Other deletion-independent findings (#13–#34) — out of scope.

---

## Unit tests (added; SQLite-backed, no docker)

In `filesystem.rs` tests (in-memory SQLite pattern from `propagator.rs`):
- `delete_file_deletes_version_row_without_version_files` — the unconditional DELETE (finding #10)
- `delete_file_etag_equality_pattern` — root == files, files_trashbin == files_trashbin/files,
  keys/versions distinct (finding #12)
- `delete_file_stamps_root_storage_mtime` — the mkdir side effect (finding #12)
- `delete_flow_materializes_cache_row` — the cache/ row (finding #8)
- `trash_directory_moves_version_subtree_and_deletes_rows` — UPDATED: dir trash keeps inner
  files' version rows (PHP by-id semantics)
- Existing tests (#6 sizes, #7 skeleton, #9 extended rows, #11 preview survival) unchanged and
  passing.

`cargo test --lib -p nc-dav` → 302 passed; workspace `cargo test --lib` clean except the
pre-existing `nc-fastcgi::registry_scans_real_apps_dir`.

## Verification (running 2026-08-07, docker free)

1. `docker compose down -v && make diff-up` — full clean reset (done; both instances ready)
2. Baseline run of `10_put_get_delete.yaml` — recorded the exact divergences above
3. Rebuild: `docker compose up -d --build nextcloud` then `docker compose restart proxy` (done)
4. Re-run `10_put_get_delete.yaml` — expect the #8/#10/#12 rows (etags, version row, root
   storage_mtime, cache row) to clear; remaining known: root size (+26, accepted #1)
5. Re-run `10_put_get.yaml` — regression guard (16.4 fixes must hold)
6. Re-run `14_propfind_depth1.yaml`, `30_share_create_selfcheck.yaml` — green-path guards
7. Re-run `17_delete_to_trash.yaml` — the full delete scenario
8. Log the outcome in `phase-16.md`: a note **below** the 16.8 task + a `## Changes` entry

---

## Open questions / risks

- **Cache-row trigger call.** The PHP read that materializes `cache/` during the delete flow is
  not identified in the PHP source (the row is replicated from live observation). If a future
  scenario reveals a different trigger context (e.g. the row appearing in a different op), the
  placement in `move_to_trash` may need revisiting.
- **Trash skeleton dir extended rows.** The `ensure_parent_dir` extended-row removal affects ALL
  callers (PUT path too) — already verified in the 16.4 PUT runs; the new mkdir side effects
  (parent storage_mtime + ancestor propagation) are PHP `View::mkdir` parity and should be no-ops
  for scenarios whose parents already exist (fast path). If a PUT/MKCOL scenario regresses, the
  mkdir side effects are the first suspect.
