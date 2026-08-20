# 06 — MOVE/COPY moved only the version FILES on disk, stranding their `files_versions/...` cache rows: Rust renamed versions without touching `oc_filecache`, unlike PHP `Cache::move`/`copyFromCache`

**Status:** fixed (uncommitted on `working` at time of writing). **Related:** [Phase 9.4](../04-tasks/phase-9.md#94-file-versions-on-overwrite-req-69--implemented) (the design this divergence comes from), [note 05](05-oidc-state-expired-session-resolve-double-login.md) (same repo/session style).

## Observable failure

After a `Photos` → `Photos2` directory rename in production, the classifier ended with **1,117 rows still under `files_versions/Photos/2024/...`** while `files_versions/Photos2/...` had **0** rows. The physical version files had moved with the primary files, but the cache did not. The PHP-FPM versions browse (`/dav/versions/...`) enumerates through the cache, so the moved versions became unaddressable.

The same rename also produced a broader cache defect: ~3,543 ghost rows at `files/Photos/2024/...` whose `parent` pointed at the still-live `files/Photos2/2024/...` directory rows. The Photos app timeline (a path-`LIKE` query) showed them; the DAV files view (enumerates by `parent`) hid them. A plain `occ files:scan` could not clear them because the scanner deletes by `parent` fileid, and those rows' `parent` is a live directory. That ghost set was cleaned out directly from `oc_filecache` (backed up first), but this note is about the version-row residue the same defect left behind.

## Root cause(s) — grounded

- Rust's `versions::rename_versions` accepted `_pool` / `_prefix` and did **only** `tokio::fs::rename` of version files; `copy_versions` did **only** `tokio::fs::copy`. Neither touched `oc_filecache` (`src/versions.rs`). When a file or directory is renamed/copied, the `files_versions/{path}.v{mtime}` rows must move/clone with their file.
- PHP operates through the cache: `Storage::renameOrCopy` (`apps/files_versions/lib/Storage.php:291`) drives `View::move`/`View::copy` on `files_versions/{old}` → `files_versions/{new}` (`:301,:306`). That lands in `Cache::move` (`lib/private/Files/Cache/Cache.php:760-831`), which rewrites the moved node's `path/path_hash/name` **and `parent`** and bulk-rewrites `path/path_hash` for descendants; `Cache::copyFromCache` (`:1223`) clones each entry to a **new fileid** with the correct new parent. Rust skipped all of it.
- The `parent` detail is load-bearing: an early draft only repathed `path/path_hash`, leaving the moved version node's `parent` dangling (the same path/parent split seen in the ghost rows). PHP also repoints `parent` on the moved node — so parity demands it there, and only there (descendants keep `parent` because they carry with the subtree).

## Options weighed

- **A. Keep disk-only handlers.** Rejected: the versions PROPFIND serves from the cache, so the move is invisible — the exact divergence observed.
- **B. Repath/clone the `oc_filecache` rows in Rust.** Chosen: mirrors PHP's observable rows for both rename (repath, preserve fileids) and copy (clone to new fileids). Must repoint `parent` on the moved/cloned node; descendants only get `path/path_hash` on rename.
- **C. Proxy version handling to PHP-FPM.** Rejected: versions are native Rust per Phase 9.4; delegating reintroduces the round-trip this rewrite exists to remove.

## The choice

`rename_versions` / `copy_versions` now take `storage_id`, `pool`, `prefix` and update `oc_filecache`:
- directory move/copy: repath/clone the whole `files_versions/{old}/...` subtree;
- file move/copy: repath/clone each `.v{ts}` row;
- repoint the moved/cloned node's `parent` from its new path (rename), or resolve new parents top-down with a fileid remap (copy).

`oc_files_versions` (the metadata table keyed by `file_id`) is deliberately **not** touched on copy — PHP creates those entities on the target's next write, not when copying — so leaving them alone is the faithful behavior.

## Verification

- `cargo test -p nc-dav --lib` — **333 passed, 0 failed**.
- New regression tests: `versions::tests::rename_versions_repaths_directory_subtree` (asserts the moved node's `parent` is repointed and fileids preserved) and `versions::tests::copy_versions_clones_version_row` (new fileid, correct parent, cloned etag/size/mtime).
- **Not yet run:** the differential A/B harness against the live PHP oracle (version PROPFIND after a live move/copy). That empirical check is the remaining verification.

## Follow-ups

1. **Lock in with the A/B harness** for a directory MOVE and COPY that carry versions, comparing the `files_versions/...` cache rows and `/dav/versions` browse between Rust (`:8080`) and PHP (`:9090`).
2. **Production cleanup** — the stranded `files_versions/Photos/2024/*` rows are residue of the old behavior; remove/rescan them once the fix is deployed (backed up first).
3. **Related file-tree paths** — `rename_subtree_paths` and `trash_directory` have the same repath-without-reparent fragility; a dedicated note/test should pin down where the upload/`bulk` path fit the incident once the producing operation is confirmed.

Back: [`../README.md`](../README.md)
