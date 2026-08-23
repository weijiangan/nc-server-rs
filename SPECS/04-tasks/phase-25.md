# Phase 25 — Write-path duplication: shared row builders

Goal: collapse the three families of copy-pasted write-path code in `nc-dav` — the `oc_filecache` INSERT, the mimetype-from-filename resolution, and the row re-key UPDATE — into shared builders. Unlike the dialect-fork and idiom collapses that preceded it, this family is **not** mechanically provable: the call sites differ in error handling, in which columns they bind, and in the side effects they trigger, so each consolidation has to be argued against the PHP call chain rather than diffed.

Prerequisites: phase-24 (24.1–24.6) for 25.4 only — see the sequencing note below. 25.1–25.3 are independent.

Site counts below were measured against the tree at `a67505f`.

---

## Sequencing — 25.4 must follow phase-24

Seven of the eleven row re-key sites live inside the six functions phase-24 rewrites:

| re-key site | phase-24 task | function |
|---|---|---|
| `mutations.rs:840` | 24.1 | `rename_subtree_paths` |
| `versions.rs:306`, `:326`, `:345` | 24.3 | `repath_version_subtree` |
| `trashbin.rs:337` | 24.4 | `trash_directory` |
| `trashbin.rs:714`, `:743` | 24.5 | `trash_versions` |

Phase-24 replaces those per-row loops with set-based bulk UPDATEs on the Postgres arm while the SQLite arm keeps the loop, so the duplication changes shape rather than disappearing. Consolidating first would be rewritten by phase-24; 25.4 is therefore gated on it.

## Tasks

### 25.1 directory-row INSERT

Four sites insert a directory row into `oc_filecache` with an identical 13-column list and identical fixed binds (`dir_mime_id`, `dir_mimepart_id`, size `0`, `mtime == storage_mtime == now`, permissions `31`, checksum `""`), varying only in path/parent/etag:

`mutations.rs:128` (`ensure_parent_dir`) · `mutations.rs:281` (`create_dir_row`) · `versions.rs:754` (version ancestor) · `cache_rows.rs:101` (`ensure_lazy_dir_row`)

Three of the four end in `RETURNING fileid`; `ensure_lazy_dir_row` instead ends in `ON CONFLICT DO NOTHING` and returns nothing, so the builder emits two statement texts, not one.

The error handling is what differs, and it is deliberate at every site: `ensure_parent_dir` treats a failed INSERT as a TOCTOU race and re-reads the row another request inserted; `create_dir_row` maps to `FsError::GeneralFailure`; the version ancestor returns a `String` error via `?`; `ensure_lazy_dir_row` logs and never returns a fileid. A shared builder must return the raw `Result` and leave every one of those policies at the call site.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 25.1.1 | `cargo test --lib`; each converted site keeps its existing error policy — assert the TOCTOU re-read in `ensure_parent_dir` still runs by unit-testing a duplicate-path insert |
| S1 | 25.1.2 | `make diff-test`; MKCOL, PUT-into-new-subdir, COPY-into-new-subdir and DELETE-to-trash scenarios show no DB-delta change |

- [ ] **25.1.1** Add a `row::insert_dir_row(...) -> Result<i64, sqlx::Error>` builder carrying the column list and the fixed dir binds; take path, parent, etag and an `on_conflict: bool` for the `ensure_lazy_dir_row` variant. Convert the four sites, leaving each one's error handling untouched.
- [ ] **25.1.2** Confirm the emitted SQL text is unchanged at all four sites (the statement text is what the PG statement-log count keys on — a changed string is a new prepared statement).

### 25.2 file-row INSERT and the clone variant

Six sites insert a non-directory row, splitting by **column list** rather than by caller:

| columns | sites | binds |
|---|---|---|
| 13 (with `checksum`) | `bulk_handler.rs:428`, `upload_handler.rs:832`, `davfile.rs:593` | fresh file: permissions `27`, one mtime value bound to both `mtime` and `storage_mtime` |
| 13 (with `checksum`) | `versions.rs:573` | full clone: every value from the source row, including distinct `src.mtime` / `src.storage_mtime` and `src.permissions` |
| 12 (no `checksum`) | `mutations.rs:664`, `versions.rs:875` | `mutations.rs` omits the column so a COPY inherits NULL, matching PHP's `copyFromCache`; `versions.rs` takes mtime, storage_mtime and permissions as separate parameters |

Two consequences for the builder, both verified against the code at `a67505f`:

- `mtime` and `storage_mtime` must be **separate** parameters. Three of the four 13-column sites bind the same value twice, but `versions.rs:573` binds `src.mtime` and `src.storage_mtime` independently, so a single-value builder would silently collapse them on the version path.
- The 12-column shape cannot be expressed as the 13-column builder with a NULL checksum. Binding NULL and omitting the column produce different statement text, and statement text is what the PG statement-log count keys on.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 25.2.1, 25.2.2 | `cargo test --lib`; a COPY unit test asserts `checksum IS NULL` on the copied row, and a version-clone test asserts `mtime != storage_mtime` survives when the source row has them differing |
| S1 | 25.2.3 | `make diff-test`; PUT, bulk upload, chunked-assembly, COPY and version-clone scenarios show no DB-delta change |

- [ ] **25.2.1** Add `row::insert_file_row(...)` for the 13-column shape, taking `mtime` and `storage_mtime` separately and `permissions` as a parameter. Convert all four 13-column sites, including the `versions.rs:573` clone.
- [ ] **25.2.2** Keep the 12-column shape as a separate `row::insert_row_no_checksum(...)` for `mutations.rs:664` and `versions.rs:875`. Confirm the emitted SQL string is byte-identical to today's at both sites before converting.
- [ ] **25.2.3** Check the `mtime`/`storage_mtime` equality on the three fresh-file sites against PHP. PHP sets `mtime` from `X-OC-MTime` when supplied but `storage_mtime` from the file's actual disk mtime; Rust binds one value to both. If PHP genuinely differs, that is a parity bug — it gets its own commit, separate from this consolidation.

### 25.3 mimetype resolution from filename

Five sites run the same three steps — `mime_guess::from_ext(ext)` → `first_raw()` defaulting to `application/octet-stream` → split on `/` for the part → two `get_or_insert_mime_id` calls:

`bulk_handler.rs:302` · `filesystem.rs:358` · `mutations.rs:398` (MOVE extension change) · `mutations.rs:633` (COPY extension change) · `upload_handler.rs:686`

The extension derivations were checked and **do agree**: `filesystem.rs` uses `file_name.rsplit('.').next()` and `mutations.rs` uses `path_utils::extension`, which is the same "last dot-segment" rule, including the no-dot case (`"Makefile"` → `"Makefile"` → no mapping → `application/octet-stream`). `filesystem.rs` additionally lowercases, which is redundant: `mime_guess::from_ext` is case-insensitive (verified against mime_guess 2.x — `"JPG"`, `"Jpg"` and `"jpg"` all resolve to `image/jpeg`). The consolidation is therefore safe, and the `.to_lowercase()` can go.

The two `get_or_insert_mime_id` calls are sequential today; they are independent and could be joined, which would remove one round trip from every write that resolves a mimetype.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 25.3.1 | `cargo test --lib`; a table-driven test pins the resolved `(mimetype, mimepart)` pair for the extensions the difftest scenarios upload, including the no-extension, unknown-extension and uppercase-extension cases |
| S1 | 25.3.2, 25.3.3 | `make diff-test`; `make perf-gate` — PUT statement count drops by at most the one joined round trip, no budget-class regression |

- [ ] **25.3.1** Add `nc_db::mime::resolve_for_filename(pool, prefix, cache, name) -> (i64, i64)` covering the five sites, using `path_utils::extension` as the single derivation and dropping the redundant `.to_lowercase()`.
- [ ] **25.3.2** Join the two `get_or_insert_mime_id` calls inside the helper so a cold mimetype costs one round trip instead of two.
- [ ] **25.3.3** Check whether the mimetype should come from `nc_db::mime`'s Nextcloud-derived mapping rather than `mime_guess`. PHP resolves via `mimetypemapping.json` + `mimetypealiases.json`; where `mime_guess` disagrees, the stored `oc_filecache.mimetype` diverges from PHP for that extension. Any disagreement found is a parity bug and gets its own commit, separate from the consolidation.

### 25.4 filecache row re-key UPDATE (after phase-24)

Eleven sites issue `UPDATE {prefix}filecache SET path=…, path_hash=…` in four shapes:

| shape | sites |
|---|---|
| `path, path_hash` by fileid | `mutations.rs:840`, `trashbin.rs:337`, `trashbin.rs:714`, `versions.rs:345` |
| `path, path_hash, name, parent` by fileid | `mutations.rs:371`, `trashbin.rs:479`, `trashbin.rs:743`, `versions.rs:306` |
| `path, path_hash, name` by fileid | `versions.rs:326` |
| by `storage` + `path` instead of fileid | `versions.rs:381`, `versions.rs:406` |

The split between the two main shapes is not arbitrary: it mirrors PHP's `Cache::moveFromCache`, where subtree children get path/path_hash only (Cache.php:749-808) and the moved node itself also gets name/parent (Cache.php:813-831). A shared builder must keep that distinction visible rather than always writing all four columns — writing `name`/`parent` on a subtree child would be a silent behaviour change.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 25.4.1 | phase-24 tasks 24.1–24.6 all ticked |
| S1 | 25.4.2 | `cargo test --lib`; the SQLite arm's per-row loop and the Postgres bulk UPDATE produce identical `path`/`path_hash`/`name`/`parent` for a 3-level subtree |
| S2 | 25.4.3 | `make diff-test`; MOVE-dir, COPY-dir and DELETE-dir-to-trash scenarios show no DB-delta change |

- [ ] **25.4.1** Re-count the re-key sites after phase-24 lands. The Postgres arms become set-based, so only the SQLite arms remain per-row — confirm how many sites and which shapes actually survive before designing anything.
- [ ] **25.4.2** Add re-key builders for whichever shapes remain, keeping the child-vs-node distinction explicit.
- [ ] **25.4.3** Convert the surviving sites.

---

## Deviations from the task descriptions

(none yet)

## Changes

Execution history only: what was tried, reverted, and why; root causes and
verification results not already stated in the task text. Nothing that merely
restates a task or the code.
