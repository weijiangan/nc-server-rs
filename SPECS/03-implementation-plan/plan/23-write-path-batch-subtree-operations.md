# 23. Write-path batch: subtree-level operations (MOVE / DELETE-to-trash)

## Context

Plan section 21's read-path work (T1-T10) collapsed depth-1 PROPFIND from
~11 queries per child to one CTE statement. Section 19's T9 collapsed the
PUT propagation path from 4 round trips to one CTE. The **write-path
subtree operations** — directory MOVE, directory DELETE-to-trash, and the
version-file relocations they cascade — were explicitly deferred:

> Write classes: MKCOL, DELETE-to-trash, MOVE, chunked assembly — capture
> their floors first, then budget them. (plan 20, "What else the gate covers")

Six methods currently do `SELECT all descendants → loop N UPDATEs`, one
round trip per child. PHP's `Cache::moveFromCache` does the same work in
one set-based UPDATE (chunked at 1000 ids, DB-side `CONCAT` + `MD5()`)
(`workspace/server/lib/private/Files/Cache/Cache.php:749-808`). The
prerequisites that made the PHP approach unreachable are now all landed:

- **T3** — native `PgPool` (Postgres has `md5()`, `||`, `SUBSTRING`).
- **T4** — stable statement text via native array binds (no `format!` churn).
- **T9** — the CTE propagation pattern is proven on the write path.

Postgres's built-in `md5()` produces the same hex digest as Rust's
`row::path_hash` (MD5 of the UTF-8 path bytes — verified
`row.rs:57-58`), so the DB-side rekey is byte-identical to the Rust-side
loop it replaces.

## The N+1 methods

All six follow the same anti-pattern: one `SELECT fileid, path … WHERE
path LIKE $prefix/%` fetch, then a per-row `UPDATE … WHERE fileid = $N`
inside a `for` loop. The fetch is necessary (the set must be known
before rekeying); the per-row UPDATEs are the N+1.

### MOVE (directory rename) — 3 methods

| # | Method | File:line | N+1 pattern |
|---|---|---|---|
| W1 | `NcFileSystem::rename_subtree_paths` | `nc-dav/src/filesystem.rs:4146` | 1 SELECT + N UPDATEs (path/path_hash per descendant) |
| W2 | `row::update_custom_properties_path_subtree` | `nc-dav/src/row.rs:1970` | 1 SELECT + N UPDATEs (propertypath per descendant in `oc_properties`) |
| W3 | `versions::repath_version_subtree` | `nc-dav/src/versions.rs:257` | 1 SELECT + N UPDATEs (path/path_hash/name/parent per version descendant) |

### DELETE-to-trash — 3 methods

| # | Method | File:line | N+1 pattern |
|---|---|---|---|
| W4 | `NcFileSystem::trash_directory` | `nc-dav/src/filesystem.rs:824` | 1 SELECT + N UPDATEs (path/path_hash per descendant rekeyed to trash prefix) |
| W5 | `NcFileSystem::trash_versions` | `nc-dav/src/filesystem.rs:1216` | 1 SELECT + N×(disk rename + UPDATE + 2× propagate_change) per version row |
| W6 | `row::delete_custom_properties_for_dir` | `nc-dav/src/row.rs:1887` | 1 SELECT + N DELETEs (oc_properties per descendant path) |

## PHP ground truth

`Cache::moveFromCache` (`Cache.php:716-853`) is the reference for W1/W3/W4:

1. `getChildIds(storageId, path)` — one `SELECT fileid … WHERE path LIKE
   $path/%` (`Cache.php:855-862`).
2. `array_chunk($childIds, 1000)` — batched at 1000 to stay under
   Doctrine's parameter limit (`Cache.php:755`).
3. One UPDATE per chunk, DB-side string construction:
   ```php
   // Cache.php:760-768
   $newPath = $fun->concat(
       $query->createNamedParameter($targetPath),
       $fun->substring('path', $query->createNamedParameter($sourceLength + 1))
   );
   $query->update('filecache')
       ->set('path_hash', $fun->md5($newPath))
       ->set('path', $newPath)
       ->whereStorageId($sourceStorageId)
       ->andWhere($query->expr()->in('fileid', $query->createParameter('files')));
   ```
4. The moved node itself is a separate single-row UPDATE (path/path_hash/
   name/parent — `Cache.php:813-831`), outside the child loop.

Postgres equivalents: `||` (string concat), `SUBSTRING(path FROM $start)`,
`md5(text)` — all standard SQL functions. SQLite has `||` and `substr()`
but no `md5()` function by default (the reason the current code hashes in
Rust). The SQLite path keeps the per-row loop behind the dialect check,
exactly as the propagator already does (`propagator.rs:169-171`).

## The fix: set-based UPDATE on the Postgres arm

Each method gets a Postgres arm that replaces the `SELECT + loop` with a
single set-based UPDATE using DB-side string construction. The pattern
mirrors PHP's `moveFromCache` and the T9 propagation CTE.

### W1/W4 — filecache descendant path rekey

```sql
UPDATE {prefix}filecache
SET path = $1 || SUBSTRING(path FROM $2),
    path_hash = md5($1 || SUBSTRING(path FROM $2))
WHERE storage = $3 AND path LIKE $4
```

`$1` = new_prefix, `$2` = old_prefix.len() + 1 (1-based, PHP's
`sourceLength + 1`), `$3` = storage_id, `$4` = `old_prefix/%`.

This is exactly `moveFromCache`'s `CONCAT(target, SUBSTRING(path,
sourceLength+1))` + `MD5(newPath)`, translated to Postgres syntax. One
statement for the whole subtree — N round trips → 1.

### W2/W6 — oc_properties path operations

`oc_properties.propertypath` stores `format_property_path(path)` — the
raw path when ≤250 chars, SHA-1 hash when >250 (`row.rs:1602-1608`).
The SHA-1 hash is Rust-side (pgcrypto is not guaranteed), so the bulk
UPDATE can only rekey paths that are **not** hashed:

```sql
-- W2 (rename subtree):
UPDATE {prefix}properties
SET propertypath = $1 || SUBSTRING(propertypath FROM $2)
WHERE userid = $3
  AND propertypath LIKE $4
  AND length(propertypath) <= 250

-- W6 (delete dir):
DELETE FROM {prefix}properties
WHERE userid = $1
  AND (propertypath = $2 OR propertypath LIKE $3)
```

The hashed-path rows (>250 chars) are rare (paths that long are
exceptional) and fall back to the per-row loop for correctness. This is a
deliberate scope boundary, not a limitation — pgcrypto could handle them
but is not guaranteed installed, and the >250 case is a negligible
fraction of real workloads.

### W3 — version subtree rekey

Same filecache rekey as W1, but on the `files_versions` subtree. The
moved node itself (the subtree root) needs name+parent too — the current
code already handles that as a special case in the loop; the bulk UPDATE
handles descendants, the root stays a single-row UPDATE (matching PHP's
`moveFromCache` separation of child-loop vs node-itself).

### W5 — trash_versions

`trash_versions` is the most complex: each version row gets a disk
rename + a filecache UPDATE + two `propagate_change` calls (source and
target chains). The disk renames are inherently per-file (each version
file moves to a distinct path), so the per-row loop's I/O cannot be
eliminated. The filecache UPDATEs, however, can be batched: collect all
`(fileid, new_path)` pairs during the disk-rename loop, then issue one
bulk UPDATE with `= ANY($1::bigint[])` + a `CASE` expression for the
path. The propagation calls can be deduplicated to one per distinct
chain (the current code already does source-chain + target-chain, but
per-row — the chains repeat).

**W5 scope decision:** the disk-rename loop stays (it's I/O-bound, not
query-bound); only the filecache UPDATEs are batched, and the
propagation is deduplicated. This halves the query count for a typical
versioned directory delete (N UPDATEs + 2N propagations → 1 UPDATE +
2 propagations).

## Execution order

**W4 → W1 → W3 → W2 → W6 → W5.**

W4 (trash_directory) is the simplest and highest-leverage: it's the
DELETE path's hot loop, and its fix is the cleanest single-UPDATE case.
W1 (rename_subtree_paths) is the same pattern on the MOVE path. W3
(version subtree) reuses W1's shape with the versions-tree prefix. W2
and W6 (oc_properties) add the hashed-path fallback. W5 (trash_versions)
is last — its disk-rename loop makes it the most complex, and batching
its UPDATEs builds on the patterns established by W1/W4.

## SQLite path

Every method keeps the current per-row loop behind the `DbPool::Sqlite`
arm. SQLite has no `md5()` function and its `substr()` is 1-based but
the overall shape differs enough that a unified SQL text isn't worth the
complexity — the SQLite path is the test path (`cargo test --lib`), and
its correctness is the parity pin for the Postgres arm.

## Verification

1. `cargo test --lib` — the existing batch-vs-single tests pin the
   Postgres arm against the SQLite arm (the same pattern T6/T7 used).
2. `make perf-gate` — W1-W6 add new budget classes: `move_dir`,
   `delete_dir_trash`, `move_dir_with_versions`. Capture the floors
   first (plan 20's "capture their floors first, then budget them").
3. `make diff-test` / `make diff-suite` — byte parity against the
   oracle (same DB state after a MOVE/DELETE).
4. A depth-2 directory MOVE scenario (nested children) in the
   differential suite, to exercise the subtree rekey end-to-end.

## Budget impact

The current write classes are unbudgeted (plan 20 lists them as future).
This work captures their floors and establishes the budgets. Expected
statement counts after batching (Postgres, warm caches):

| class | current (N children) | after batching |
|---|---|---|
| `move_dir` | ~12 + N (subtree rekey + versions + custom props + propagation) | ~12 + 3 (one bulk UPDATE per table: filecache, properties, versions) |
| `delete_dir_trash` | ~10 + N (trash rekey + versions + propagation) | ~10 + 2 (trash rekey + versions batched; custom props is already one DELETE) |

The scaling delta for write classes should be 0 (no per-child
statements), mirroring the read path's keystone invariant.
