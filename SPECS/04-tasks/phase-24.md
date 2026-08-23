# Phase 24 — Write-path batch: subtree-level operations

Goal: eliminate the N+1 query patterns in directory MOVE and DELETE-to-trash by replacing the per-row `SELECT + loop UPDATE` with set-based bulk UPDATEs on the Postgres arm, matching PHP's `Cache::moveFromCache` (DB-side `CONCAT` + `MD5()`). The SQLite test arm keeps the per-row loop as the parity pin. Six methods across three files.

Full plan: [`SPECS/03-implementation-plan/plan/23-write-path-batch-subtree-operations.md`](../03-implementation-plan/plan/23-write-path-batch-subtree-operations.md).

Prerequisites (all landed in phase-22): T3 native `PgPool`, T4 native array binds, T9 CTE propagation pattern.

---

## W1 — filecache descendant path rekey on directory MOVE (`filesystem.rs:4146`)

`NcFileSystem::rename_subtree_paths` does `SELECT fileid, path WHERE path LIKE $old/%` → N per-row `UPDATE … SET path=$1, path_hash=$2 WHERE fileid=$3`. PHP's `moveFromCache` (Cache.php:749-808) does the same work in one set-based UPDATE with DB-side `CONCAT(target, SUBSTRING(path, sourceLen+1))` + `MD5(newPath)`.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | W1.1 | `cargo test --lib` (SQLite arm unchanged — the parity pin); new test: a 3-level nested directory MOVE on Postgres produces the same `path`/`path_hash` values as the SQLite arm |
| S1 | W1.2 | `make diff-test`; `make perf-gate` — capture `move_dir` floor |

- [ ] **W1.1** Add a Postgres arm to `rename_subtree_paths`: one bulk `UPDATE {prefix}filecache SET path = $1 || SUBSTRING(path FROM $2), path_hash = md5($1 || SUBSTRING(path FROM $2)) WHERE storage = $3 AND path LIKE $4`. The SQLite arm keeps the per-row loop. `$2` = `old_prefix.len() + 1` (1-based — PHP's `sourceLength + 1`).
- [ ] **W1.2** Add a `move_dir` budget class to `perf-budget.yaml`; capture the measured floor (expected: the subtree rekey is 1 statement, not N).

## W2 — oc_properties path subtree update on MOVE (`row.rs:1970`)

`update_custom_properties_path_subtree` does `SELECT path FROM filecache WHERE path LIKE $old/%` → N per-row `UPDATE oc_properties SET propertypath=$1 WHERE userid=$2 AND propertypath=$3`. `propertypath` stores `format_property_path(path)` — the raw path when ≤250 chars, SHA-1 hash when >250 (`row.rs:1602-1608`). The SHA-1 hash is Rust-side (pgcrypto not guaranteed), so the bulk UPDATE can only rekey non-hashed paths.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | W2.1 | `cargo test --lib` (SQLite arm unchanged); new test: a subtree MOVE with mixed-length custom property paths (some ≤250, some >250) produces the same `oc_properties` state on both arms |
| S1 | W2.2 | `make diff-test`; perf-gate — `move_dir` floor holds |

- [ ] **W2.1** Add a Postgres arm: one bulk `UPDATE {prefix}properties SET propertypath = $1 || SUBSTRING(propertypath FROM $2) WHERE userid = $3 AND propertypath LIKE $4 AND length(propertypath) <= 250`. Hashed-path rows (>250 chars, rare) fall back to the per-row loop. The SQLite arm keeps the per-row loop entirely.
- [ ] **W2.2** Extend the `move_dir` perf-gate to cover custom properties (if not already captured by W1.2's floor).

## W3 — version subtree rekey on MOVE (`versions.rs:257`)

`repath_version_subtree` does `SELECT fileid, path WHERE path = $old OR path LIKE $old/%` → N per-row UPDATEs. Descendants get path/path_hash; the moved node itself (subtree root) gets path/path_hash/name/parent — matching PHP's `moveFromCache` separation of child-loop vs node-itself (Cache.php:749-808 vs 813-831). Same set-based shape as W1.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | W3.1 | `cargo test --lib` (SQLite arm unchanged); new test: a version directory subtree MOVE on Postgres matches the SQLite arm's path/path_hash/name/parent values |
| S1 | W3.2 | `make diff-test`; perf-gate — `move_dir_with_versions` floor captured |

- [ ] **W3.1** Add a Postgres arm: bulk `UPDATE {prefix}filecache SET path = $1 || SUBSTRING(path FROM $2), path_hash = md5($1 || SUBSTRING(path FROM $2)) WHERE storage = $3 AND path LIKE $4` for descendants; the subtree root stays a single-row UPDATE (path/path_hash/name/parent) — it's one row, not an N+1. The SQLite arm keeps the per-row loop.
- [ ] **W3.2** Add a `move_dir_with_versions` budget class (or fold into `move_dir` if the version rekey is a fixed +1 over the filecache rekey).

## W4 — filecache descendant path rekey on DELETE-to-trash (`filesystem.rs:824`)

`trash_directory` does `SELECT fileid, path WHERE path LIKE $fc/%` → N per-row `UPDATE … SET path=$1, path_hash=$2 WHERE fileid=$3` to rekey descendants into the trash prefix. Same set-based shape as W1 — the trash prefix is just a different `new_prefix`.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | W4.1 | `cargo test --lib` (SQLite arm unchanged); new test: a directory DELETE-to-trash on Postgres produces the same descendant paths/hashes as the SQLite arm |
| S1 | W4.2 | `make diff-test`; perf-gate — `delete_dir_trash` floor captured |

- [ ] **W4.1** Add a Postgres arm: one bulk `UPDATE {prefix}filecache SET path = $1 || SUBSTRING(path FROM $2), path_hash = md5($1 || SUBSTRING(path FROM $2)) WHERE storage = $3 AND path LIKE $4` where `$1` = trash_fc. The SQLite arm keeps the per-row loop.
- [ ] **W4.2** Add a `delete_dir_trash` budget class; capture the measured floor (the descendant rekey is 1 statement, not N).

## W5 — trash_versions batched UPDATEs (`filesystem.rs:1216`)

`trash_versions` does `SELECT fileid, path …` → per-row: disk rename + filecache UPDATE + 2× `propagate_change`. The disk renames are inherently per-file (each version file moves to a distinct path), so the per-row I/O loop stays. But the filecache UPDATEs can be collected and issued as one bulk UPDATE, and the propagation calls can be deduplicated to one per distinct chain.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | W5.1 | `cargo test --lib`; new test: a directory with multiple version files trashed on Postgres produces the same filecache state as the SQLite arm |
| S1 | W5.2 | `make diff-test`; perf-gate — `delete_dir_trash` floor drops |

- [ ] **W5.1** Collect `(fileid, new_path, new_name, new_parent)` during the disk-rename loop; issue one bulk UPDATE for subtree children (`path`/`path_hash` only) and one for the moved nodes (path/path_hash/name/parent — or a `CASE` expression if the shapes differ). Deduplicate propagation: one `propagate_change` per distinct source chain and one per distinct target chain, instead of 2N calls.
- [ ] **W5.2** Re-measure `delete_dir_trash` — the version-related query count should drop from ~N to ~1 for the filecache UPDATEs, and propagation from ~2N to ~2.

## W6 — oc_properties delete for directory (`row.rs:1887`)

`delete_custom_properties_for_dir` does `SELECT path FROM filecache WHERE path = $dir OR path LIKE $dir/%` → N per-row `DELETE FROM oc_properties WHERE userid=$1 AND propertypath=$2`. The `propertypath` column stores `format_property_path(path)` (≤250 raw, >250 SHA-1 hashed). For the delete case, the hashed-path rows can be handled with a `path_hash`-based join: `DELETE FROM oc_properties p USING {prefix}filecache fc WHERE fc.storage = $1 AND (fc.path = $2 OR fc.path LIKE $3) AND p.userid = $4 AND p.propertypath = format_property_path(fc.path)` — but `format_property_path` is Rust-side, so the join isn't expressible in pure SQL without pgcrypto. The non-hashed rows (>250) are a rare fallback.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | W6.1 | `cargo test --lib` (SQLite arm unchanged); new test: a directory DELETE with custom properties on children produces the same `oc_properties` deletion on both arms |
| S1 | W6.2 | `make diff-test`; perf-gate — `delete_dir_trash` floor holds |

- [ ] **W6.1** Add a Postgres arm: one bulk `DELETE FROM {prefix}properties WHERE userid = $1 AND (propertypath = $2 OR propertypath LIKE $3) AND length(propertypath) <= 250` for the non-hashed paths. The hashed-path fallback stays as the per-row loop (rare). Consider whether the existing `DELETE FROM oc_properties WHERE userid=$1 AND propertypath = $path` can instead join on `filecache` to avoid the `format_property_path` incompatibility entirely — if the filecache rows are already being deleted (hard-delete path), the `oc_properties` rows are orphaned and can be cleaned with a `USING` join on the storage+path LIKE.
- [ ] **W6.2** Verify the `delete_dir_trash` floor accounts for the `oc_properties` cleanup (1 statement, not N).

---

## Deviations from the task descriptions

(none yet)

## Changes

Execution history only: what was tried, reverted, and why; root causes and
verification results not already stated in the task text. Nothing that merely
restates a task or the code.
