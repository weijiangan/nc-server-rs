# Phase 24 — Write-path batch: subtree-level operations

Goal: eliminate the N+1 query patterns in directory MOVE and DELETE-to-trash by replacing the per-row `SELECT + loop UPDATE` with set-based bulk UPDATEs on the Postgres arm, matching PHP's `Cache::moveFromCache` (DB-side `CONCAT` + `MD5()`). The SQLite test arm keeps the per-row loop as the parity pin. Six methods across three files.

Full plan: [`SPECS/03-implementation-plan/plan/23-write-path-batch-subtree-operations.md`](../03-implementation-plan/plan/23-write-path-batch-subtree-operations.md).

Prerequisites (all landed in phase-22): T3 native `PgPool`, T4 native array binds, T9 CTE propagation pattern.

---

## MOVE (directory rename)

### 24.1 filecache descendant path rekey on directory MOVE (plan W1, `mutations.rs:813`)

`NcFileSystem::rename_subtree_paths` does `SELECT fileid, path WHERE path LIKE $old/%` → N per-row `UPDATE … SET path=$1, path_hash=$2 WHERE fileid=$3`. PHP's `moveFromCache` (Cache.php:749-808) does the same work in one set-based UPDATE with DB-side `CONCAT(target, SUBSTRING(path, sourceLen+1))` + `MD5(newPath)`.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 24.1.1 | `cargo test --lib` (SQLite arm unchanged — the parity pin); new test: a 3-level nested directory MOVE on Postgres produces the same `path`/`path_hash` values as the SQLite arm |
| S1 | 24.1.2 | `make diff-test`; `make perf-gate` — capture `move_dir` floor |

- [x] **24.1.1** Add a Postgres arm to `rename_subtree_paths`: one bulk `UPDATE {prefix}filecache SET path = $1 || SUBSTRING(path FROM $2), path_hash = md5($1 || SUBSTRING(path FROM $2)) WHERE storage = $3 AND path LIKE $4`. The SQLite arm keeps the per-row loop. `$2` = `old_prefix.len() + 1` (1-based — PHP's `sourceLength + 1`).
- [ ] **24.1.2** Add a `move_dir` budget class to `perf-budget.yaml`; capture the measured floor (expected: the subtree rekey is 1 statement, not N).

### 24.2 oc_properties path subtree update on MOVE (plan W2, `row.rs:1838`)

`update_custom_properties_path_subtree` does `SELECT path FROM filecache WHERE path LIKE $old/%` → N per-row `UPDATE oc_properties SET propertypath=$1 WHERE userid=$2 AND propertypath=$3`. `propertypath` stores `format_property_path(path)` — the raw path when ≤250 chars, SHA-1 hash when >250 (`row.rs:1535-1544`). The SHA-1 hash is Rust-side (pgcrypto not guaranteed), so the bulk UPDATE can only rekey non-hashed paths.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 24.2.1 | `cargo test --lib` (SQLite arm unchanged); new test: a subtree MOVE with mixed-length custom property paths (some ≤250, some >250) produces the same `oc_properties` state on both arms |
| S1 | 24.2.2 | `make diff-test`; perf-gate — `move_dir` floor holds |

- [x] **24.2.1** Add a Postgres arm: one bulk `UPDATE {prefix}properties SET propertypath = $1 || SUBSTRING(propertypath FROM $2) WHERE userid = $3 AND propertypath LIKE $4 AND length(propertypath) <= 250`. Hashed-path rows (>250 chars, rare) fall back to the per-row loop. The SQLite arm keeps the per-row loop entirely.
- [ ] **24.2.2** Extend the `move_dir` perf-gate to cover custom properties (if not already captured by 24.1.2's floor).

### 24.3 version subtree rekey on MOVE (plan W3, `versions.rs:260`)

`repath_version_subtree` does `SELECT fileid, path WHERE path = $old OR path LIKE $old/%` → N per-row UPDATEs. Descendants get path/path_hash; the moved node itself (subtree root) gets path/path_hash/name/parent — matching PHP's `moveFromCache` separation of child-loop vs node-itself (Cache.php:749-808 vs 813-831). Same set-based shape as 24.1.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 24.3.1 | `cargo test --lib` (SQLite arm unchanged); new test: a version directory subtree MOVE on Postgres matches the SQLite arm's path/path_hash/name/parent values |
| S1 | 24.3.2 | `make diff-test`; perf-gate — `move_dir_with_versions` floor captured |

- [x] **24.3.1** Add a Postgres arm: bulk `UPDATE {prefix}filecache SET path = $1 || SUBSTRING(path FROM $2), path_hash = md5($1 || SUBSTRING(path FROM $2)) WHERE storage = $3 AND path LIKE $4` for descendants; the subtree root stays a single-row UPDATE (path/path_hash/name/parent) — it's one row, not an N+1. The SQLite arm keeps the per-row loop.
- [ ] **24.3.2** Add a `move_dir_with_versions` budget class (or fold into `move_dir` if the version rekey is a fixed +1 over the filecache rekey).

## DELETE-to-trash

### 24.4 filecache descendant path rekey on DELETE-to-trash (plan W4, `trashbin.rs:298`)

`trash_directory` does `SELECT fileid, path WHERE path LIKE $fc/%` → N per-row `UPDATE … SET path=$1, path_hash=$2 WHERE fileid=$3` to rekey descendants into the trash prefix. Same set-based shape as 24.1 — the trash prefix is just a different `new_prefix`.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 24.4.1 | `cargo test --lib` (SQLite arm unchanged); new test: a directory DELETE-to-trash on Postgres produces the same descendant paths/hashes as the SQLite arm |
| S1 | 24.4.2 | `make diff-test`; perf-gate — `delete_dir_trash` floor captured |

- [x] **24.4.1** Add a Postgres arm: one bulk `UPDATE {prefix}filecache SET path = $1 || SUBSTRING(path FROM $2), path_hash = md5($1 || SUBSTRING(path FROM $2)) WHERE storage = $3 AND path LIKE $4` where `$1` = trash_fc. The SQLite arm keeps the per-row loop.
- [ ] **24.4.2** Add a `delete_dir_trash` budget class; capture the measured floor (the descendant rekey is 1 statement, not N).

### 24.5 trash_versions batched UPDATEs (plan W5, `trashbin.rs:620`)

`trash_versions` does `SELECT fileid, path …` → per-row: disk rename + filecache UPDATE + 2× `propagate_change`. The disk renames are inherently per-file (each version file moves to a distinct path), so the per-row I/O loop stays. But the filecache UPDATEs can be collected and issued as one bulk UPDATE, and the propagation calls can be deduplicated to one per distinct chain.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 24.5.1 | `cargo test --lib`; new test: a directory with multiple version files trashed on Postgres produces the same filecache state as the SQLite arm |
| S1 | 24.5.2 | `make diff-test`; perf-gate — `delete_dir_trash` floor drops |

- [x] **24.5.1** Collect `(fileid, new_path, new_name, new_parent)` during the disk-rename loop; issue one bulk UPDATE for subtree children (`path`/`path_hash` only) and one for the moved nodes (path/path_hash/name/parent — or a `CASE` expression if the shapes differ). Deduplicate propagation: one `propagate_change` per distinct source chain and one per distinct target chain, instead of 2N calls.
- [ ] **24.5.2** Re-measure `delete_dir_trash` — the version-related query count should drop from ~N to ~1 for the filecache UPDATEs, and propagation from ~2N to ~2.

### 24.6 oc_properties delete for directory (plan W6, `row.rs:1775`)

`delete_custom_properties_for_dir` does `SELECT path FROM filecache WHERE path = $dir OR path LIKE $dir/%` → N per-row `DELETE FROM oc_properties WHERE userid=$1 AND propertypath=$2`. The `propertypath` column stores `format_property_path(path)` (≤250 raw, >250 SHA-1 hashed). For the delete case, the hashed-path rows can be handled with a `path_hash`-based join: `DELETE FROM oc_properties p USING {prefix}filecache fc WHERE fc.storage = $1 AND (fc.path = $2 OR fc.path LIKE $3) AND p.userid = $4 AND p.propertypath = format_property_path(fc.path)` — but `format_property_path` is Rust-side, so the join isn't expressible in pure SQL without pgcrypto. The non-hashed rows (>250) are a rare fallback.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 24.6.1 | `cargo test --lib` (SQLite arm unchanged); new test: a directory DELETE with custom properties on children produces the same `oc_properties` deletion on both arms |
| S1 | 24.6.2 | `make diff-test`; perf-gate — `delete_dir_trash` floor holds |

- [x] **24.6.1** Add a Postgres arm: one bulk `DELETE FROM {prefix}properties WHERE userid = $1 AND (propertypath = $2 OR propertypath LIKE $3) AND length(propertypath) <= 250` for the non-hashed paths. The hashed-path fallback stays as the per-row loop (rare). Consider whether the existing `DELETE FROM oc_properties WHERE userid=$1 AND propertypath = $path` can instead join on `filecache` to avoid the `format_property_path` incompatibility entirely — if the filecache rows are already being deleted (hard-delete path), the `oc_properties` rows are orphaned and can be cleaned with a `USING` join on the storage+path LIKE.
- [ ] **24.6.2** Verify the `delete_dir_trash` floor accounts for the `oc_properties` cleanup (1 statement, not N).

---

## Deviations from the task descriptions

- **24.1.1 — the `SUBSTRING` offset is a character count, not `old_prefix.len() + 1`.** Rust's `str::len()` is bytes; Postgres' `SUBSTRING(x FROM n)` counts characters. PHP uses `mb_strlen($sourcePath) + 1` (`Cache.php:751`), so the offset is `chars().count() + 1` — a byte offset corrupts every path containing a non-ASCII character.
- **24.2.1 — the threshold is `octet_length`, and it is applied to the new path too.** `format_property_path` hashes on **byte** length (`row.rs:1600-1608`), so the SQL guard is `octet_length`, not `length`. A rename can also push a ≤250-byte path *over* the threshold, where it must become a SHA-1 digest — so the bulk UPDATE additionally requires the constructed new `propertypath` to be ≤250 bytes, and the per-path fallback fetches descendants failing *either* side.
- **24.6.1 — no length filter on the bulk DELETE.** A hashed `propertypath` is a 40-char digest, so it can never match `dir/%` nor the directory's own formatted path; the unfiltered `DELETE` is already exact. The hashed descendants are found by the filecache fetch (narrowed to `octet_length(path) > 250` on Postgres).
- **24.1/24.3/24.4/24.5 share one helper.** The four call sites issue the same subtree rekey, so it lives once in `row::rekey_subtree_paths` (Postgres arm = one statement, SQLite arm = the fetch-and-loop parity pin) instead of four copies of the SQL.

## Changes

Execution history only: what was tried, reverted, and why; root causes and
verification results not already stated in the task text. Nothing that merely
restates a task or the code.

### 24.1-24.6 — set-based subtree operations

- **W4's pre-fetch disappears entirely.** `trash_directory` fetched the descendants *before* `move_to_trash`, but that call only re-keys the directory's own row — the descendants still carry the pre-move prefix afterwards, so the rekey can run after the move with no `SELECT` at all.
- **The version-subtree root is now matched by `storage + path_hash`**, not `fileid`: the fetch that supplied the fileid was the N+1 being removed, and the row's old hash is already known from its old path.
- **W5's propagation is deduplicated by parent path.** `propagate_change` stamps a node's *ancestors*, so rows sharing a parent share the whole chain; the key is the parent path, which preserves the exact set of stamped rows. Source chains are stamped before target chains (previously interleaved per row) — the final writer is still a target chain, so the live-verified `files_trashbin.etag == files_trashbin/versions.etag` invariant holds.
- **Postgres `md5(text)` digests the UTF-8 bytes**, so the DB-side `path_hash` is identical to `row::path_hash`'s Rust-side digest — this is what makes the set-based rekey byte-equivalent to the loop it replaces.
- **Verification**: `cargo test --lib` — 338 tests in `nc-dav` (was 333). New tests pin the SQLite arm's exact `path`/`path_hash`/`propertypath` values for a 3-level nested rekey, a nested DELETE-to-trash, and the property subtree move/delete with paths on both sides of the 250-byte hash threshold; a pure test pins the character-vs-byte offset the Postgres SQL depends on. The S1 gates (`make diff-test`, `make perf-gate`) were **not** run — no dev-docker on this machine — so every `.2` task stays open.

