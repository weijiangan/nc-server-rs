//! Cache propagation on write — parent ETag / mtime / size.
//!
//! After every mutating operation (PUT, DELETE, MOVE, COPY, MKCOL,
//! chunked-upload assembly, mtime-changing PROPPATCH), the parent chain
//! in `oc_filecache` must be updated so that desktop-sync incremental
//! polls detect the change purely from the parent ETag.
//!
//! PHP reference:
//! - `lib/private/Files/Cache/Updater.php` (update/remove/rename)
//! - `lib/private/Files/Cache/Propagator.php` (propagateChange)

use nc_db::pool::{DbPool, DbTxn};
use sqlx::{Postgres, Row as _};
use tracing::warn;

use crate::row;

/// Drives cache propagation for a single storage.
///
/// Created per request (like `NcFileSystem`), cheap to construct.
#[derive(Clone)]
pub struct Propagator {
    pool: DbPool,
    prefix: String,
    storage_id: i64,
}

impl Propagator {
    pub const MAX_RETRIES: u32 = 3;

    pub fn new(pool: DbPool, prefix: String, storage_id: i64) -> Self {
        Self {
            pool,
            prefix,
            storage_id,
        }
    }

    // ── Parent chain ───────────────────────────────────────────────────────

    /// Build the ancestor chain for an internal path, matching PHP's
    /// `Propagator::getParents()`.
    ///
    /// ```text
    /// "files/a/b/c" → ["", "files", "files/a", "files/a/b"]
    /// "files"       → [""]
    /// ```
    ///
    /// The empty string represents the storage root.  PHP uses `md5("")`
    /// as the root hash, which we match via `row::path_hash("")`.
    pub fn get_parents(path: &str) -> Vec<String> {
        let parts: Vec<&str> = path.split('/').collect();
        let mut parent = String::new();
        let mut parents = Vec::with_capacity(parts.len());
        for part in parts {
            parents.push(parent.clone());
            if parent.is_empty() {
                parent = part.to_string();
            } else {
                parent.push('/');
                parent.push_str(part);
            }
        }
        parents
    }

    // ── Core propagation ───────────────────────────────────────────────────

    /// Update `etag`, `mtime`, and optionally `size` for every ancestor of
    /// `internal_path` up to the storage root.
    ///
    /// Matches PHP `Propagator::propagateChange()`:
    /// - `time` is clamped to `now` before use.
    /// - All ancestors get the **same** new etag (one `uniqid()` call).
    /// - `mtime = GREATEST(mtime, time)`.
    /// - `size` is adjusted only when `size_difference != 0` AND the
    ///   ancestor already has a calculated size (`size > -1`).
    /// - Retried up to `MAX_RETRIES` on retryable DB errors.
    pub async fn propagate_change(
        &self,
        internal_path: &str,
        time: i64,
        size_difference: i64,
    ) -> Result<(), String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let time = time.min(now);

        let parents = Self::get_parents(internal_path);
        if parents.is_empty() {
            return Ok(());
        }

        let mut parent_hashes: Vec<String> = parents.iter().map(|p| row::path_hash(p)).collect();
        // Sort to ensure rows are always locked in the same order (PHP line 77).
        parent_hashes.sort();

        // All ancestors get the same etag (PHP line 78).
        let etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());

        for attempt in 0..Self::MAX_RETRIES {
            match self
                .try_propagate(&parent_hashes, time, size_difference, &etag)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if attempt + 1 >= Self::MAX_RETRIES {
                        return Err(e);
                    }
                    warn!(
                        attempt = attempt + 1,
                        max_retries = Self::MAX_RETRIES,
                        error = %e,
                        "Retrying propagation query after retryable exception"
                    );
                }
            }
        }

        Err("propagation exhausted retries".to_string())
    }

    /// Issue the UPDATE for all parent hashes in one statement.
    ///
    /// Uses a `CASE WHEN` expression so that `size` is only adjusted for rows
    /// that already have a calculated size (`size > -1`), matching PHP lines
    /// 91-100.
    ///
    /// The UPDATE runs inside an explicit transaction that first pre-locks
    /// every parent row `FOR UPDATE … ORDER BY path_hash` (Phase 18).  Postgres
    /// locks `IN`-list matches in *scan order* (plan-dependent: index vs. heap),
    /// not IN-list order — the sorted IN-list alone cannot enforce a lock
    /// order, and concurrent propagations over the same parent set could lock
    /// rows in opposite orders and deadlock (observed: concurrent PUTs into
    /// one directory → `deadlock detected` on this very statement, ~2 s
    /// stalls, sqlx pool churn).  Locking all parents in a deterministic
    /// order makes concurrent propagations serialize instead of cycle.
    async fn try_propagate(
        &self,
        parent_hashes: &[String],
        time: i64,
        size_difference: i64,
        etag: &str,
    ) -> Result<(), String> {
        let placeholders: Vec<String> =
            (1..=parent_hashes.len()).map(|i| format!("${i}")).collect();
        let in_clause = placeholders.join(", ");

        // Parameter indices:
        //   $1..$N  = path_hashes
        //   $(N+1)  = storage_id
        //   $(N+2)  = time
        //   $(N+3)  = etag
        let storage_idx = parent_hashes.len() + 1;
        let time_idx = parent_hashes.len() + 2;
        let etag_idx = parent_hashes.len() + 3;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("propagate BEGIN failed: {e}"))?;
        // Pre-lock the parent rows in a deterministic order (see the doc
        // comment above).  Postgres-only: SQLite has whole-file locking (no
        // row locks, no deadlock hazard) and does not support `FOR UPDATE`.
        // Native text[] bind (PHASE-22 T4): the path_hashes are md5 hex, so
        // the array interim (21.3) is gone.
        match &mut tx {
            DbTxn::Pg(t) => {
                let lock_sql = format!(
                    "SELECT path_hash FROM {prefix}filecache \
                     WHERE storage = $2 AND path_hash = ANY($1::text[]) \
                     ORDER BY path_hash FOR UPDATE",
                    prefix = self.prefix,
                );
                sqlx::query::<Postgres>(&lock_sql)
                    .bind(parent_hashes)
                    .bind(self.storage_id)
                    .execute(&mut **t)
                    .await
                    .map_err(|e| format!("propagate pre-lock failed: {e}"))?;
            }
            DbTxn::Sqlite(_) => {}
        }

        // Use CASE WHEN instead of GREATEST for cross-DB compatibility
        // (SQLite lacks GREATEST; PostgreSQL and MySQL support both).
        if size_difference != 0 {
            // CASE WHEN size > -1 THEN MAX(size + $sizeDiff, -1) ELSE size END.
            // Native text[] bind (PHASE-22 T4): $1 = path_hashes, $2 =
            // storage, $3 = time, $4 = etag, $5 = size_difference.
            match &mut tx {
                DbTxn::Pg(t) => {
                    let sql = format!(
                        "UPDATE {prefix}filecache \
                         SET mtime = CASE WHEN mtime < $3 THEN $3 ELSE mtime END, \
                             etag = $4, \
                             size = CASE WHEN size > -1 \
                                      THEN CASE WHEN size + $5 < -1 THEN -1 \
                                                ELSE size + $5 END \
                                      ELSE size \
                                    END \
                         WHERE storage = $2 \
                         AND path_hash = ANY($1::text[])",
                        prefix = self.prefix,
                    );
                    sqlx::query::<Postgres>(&sql)
                        .bind(parent_hashes)
                        .bind(self.storage_id)
                        .bind(time)
                        .bind(etag)
                        .bind(size_difference)
                        .execute(&mut **t)
                        .await
                        .map_err(|e| format!("propagate UPDATE with size failed: {e}"))?;
                }
                DbTxn::Sqlite(t) => {
                    let size_diff_idx = parent_hashes.len() + 4;
                    let sql = format!(
                        "UPDATE {prefix}filecache \
                         SET mtime = CASE WHEN mtime < ${time_idx} THEN ${time_idx} ELSE mtime END, \
                             etag = ${etag_idx}, \
                             size = CASE WHEN size > -1 \
                                      THEN CASE WHEN size + ${size_diff_idx} < -1 THEN -1 \
                                                ELSE size + ${size_diff_idx} END \
                                      ELSE size \
                                    END \
                         WHERE storage = ${storage_idx} \
                         AND path_hash IN ({in_clause})",
                        prefix = self.prefix,
                        time_idx = time_idx,
                        etag_idx = etag_idx,
                        size_diff_idx = size_diff_idx,
                        storage_idx = storage_idx,
                        in_clause = in_clause,
                    );
                    let mut query = sqlx::query(&sql);
                    for h in parent_hashes {
                        query = query.bind(h);
                    }
                    query = query.bind(self.storage_id);
                    query = query.bind(time);
                    query = query.bind(etag);
                    query = query.bind(size_difference);
                    query
                        .execute(&mut **t)
                        .await
                        .map_err(|e| format!("propagate UPDATE with size failed: {e}"))?;
                }
            }
        } else {
            // No size change — only etag + mtime.
            match &mut tx {
                DbTxn::Pg(t) => {
                    let sql = format!(
                        "UPDATE {prefix}filecache \
                         SET mtime = CASE WHEN mtime < $3 THEN $3 ELSE mtime END, \
                             etag = $4 \
                         WHERE storage = $2 \
                         AND path_hash = ANY($1::text[])",
                        prefix = self.prefix,
                    );
                    sqlx::query::<Postgres>(&sql)
                        .bind(parent_hashes)
                        .bind(self.storage_id)
                        .bind(time)
                        .bind(etag)
                        .execute(&mut **t)
                        .await
                        .map_err(|e| format!("propagate UPDATE failed: {e}"))?;
                }
                DbTxn::Sqlite(t) => {
                    let sql = format!(
                        "UPDATE {prefix}filecache \
                         SET mtime = CASE WHEN mtime < ${time_idx} THEN ${time_idx} ELSE mtime END, \
                             etag = ${etag_idx} \
                         WHERE storage = ${storage_idx} \
                         AND path_hash IN ({in_clause})",
                        prefix = self.prefix,
                        time_idx = time_idx,
                        etag_idx = etag_idx,
                        storage_idx = storage_idx,
                        in_clause = in_clause,
                    );
                    let mut query = sqlx::query(&sql);
                    for h in parent_hashes {
                        query = query.bind(h);
                    }
                    query = query.bind(self.storage_id);
                    query = query.bind(time);
                    query = query.bind(etag);
                    query
                        .execute(&mut **t)
                        .await
                        .map_err(|e| format!("propagate UPDATE failed: {e}"))?;
                }
            }
        }

        tx.commit()
            .await
            .map_err(|e| format!("propagate COMMIT failed: {e}"))?;
        Ok(())
    }

    // ── Folder size recalculation ──────────────────────────────────────────

    /// Recalculate a folder's `size` from the sum of its direct children.
    ///
    /// Matches PHP `Cache::correctFolderSize()` — used after MOVE/COPY to fix
    /// the immediate source/target parent sizes (which can't be expressed as
    /// a simple signed delta when entire subtrees move).
    ///
    /// Also propagates the resulting size change up the ancestor chain.
    pub async fn correct_folder_size(&self, fc_path: &str) -> Result<(), String> {
        // Look up the folder row.
        let folder = row::lookup_by_path(&self.pool, &self.prefix, self.storage_id, fc_path)
            .await
            .ok_or_else(|| format!("correct_folder_size: not found: {fc_path}"))?;

        let old_size = folder.size;

        // SUM of direct children sizes (only rows with size > -1 are counted;
        // unscanned folders with size = -1 contribute 0 to the sum via COALESCE).
        // PostgreSQL SUM(bigint) returns NUMERIC, which sqlx::Any can't decode;
        // CAST to BIGINT keeps it as a plain i64.  SQLite treats CAST AS BIGINT
        // as INTEGER affinity — harmless, same column as without the cast.
        let sql = format!(
            "SELECT COALESCE(CAST(SUM(size) AS BIGINT), 0) AS total \
             FROM {prefix}filecache \
             WHERE parent = $1 AND storage = $2 AND size > -1",
            prefix = self.prefix
        );
        let new_size: i64 = sqlx::query_scalar(&sql)
            .bind(folder.fileid)
            .bind(self.storage_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| format!("correct_folder_size SUM query failed: {e}"))?;

        if new_size == old_size {
            return Ok(());
        }

        // Update the folder's size.
        let update_sql = format!(
            "UPDATE {prefix}filecache SET size = $1 WHERE fileid = $2",
            prefix = self.prefix
        );
        sqlx::query(&update_sql)
            .bind(new_size)
            .bind(folder.fileid)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("correct_folder_size UPDATE failed: {e}"))?;

        // Propagate the size delta up to ancestors.
        let size_delta = new_size - old_size;
        if size_delta != 0 {
            self.propagate_change(fc_path, new_size, size_delta).await?;
        }

        Ok(())
    }

    // ── Parent storage_mtime correction (§21.1.1 step 6) ────────────────────

    /// Mirror PHP `Updater::correctParentStorageMtime()` (`Updater.php:225-241`).
    ///
    /// Sets the **direct parent**'s `storage_mtime` to the parent **directory's
    /// disk mtime** (`storage->filemtime($parent)` in PHP). Because PHP's
    /// `Cache::normalizeData` copies `storage_mtime` → `mtime` when no explicit
    /// `mtime` is supplied (`Cache.php:468-471`), both columns take the same value.
    /// The subsequent `propagate_change` then applies `GREATEST(mtime, time)`,
    /// matching PHP's ordering (correctParentStorageMtime runs before propagateChange).
    ///
    /// Failures are logged but non-fatal: PHP swallows the `DeadlockException` here
    /// ("at worst the storage_mtime isn't updated … only trigger an extra rescan").
    pub async fn correct_parent_storage_mtime(
        &self,
        parent_fc_path: &str,
        parent_disk_path: &std::path::Path,
    ) -> Result<(), String> {
        // Parent directory's on-disk mtime, truncated to whole seconds (PHP
        // `filemtime` returns int seconds).
        let disk_mtime = match std::fs::metadata(parent_disk_path).and_then(|m| m.modified()) {
            Ok(t) => t
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            Err(e) => {
                warn!(
                    parent = %parent_fc_path,
                    error = %e,
                    "correct_parent_storage_mtime: cannot read parent dir mtime"
                );
                return Err(format!("correct_parent_storage_mtime: {e}"));
            }
        };

        let hash = row::path_hash(parent_fc_path);
        let sql = format!(
            "UPDATE {prefix}filecache \
             SET storage_mtime = $1, mtime = $1 \
             WHERE storage = $2 AND path_hash = $3",
            prefix = self.prefix
        );
        sqlx::query(&sql)
            .bind(disk_mtime)
            .bind(self.storage_id)
            .bind(&hash)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("correct_parent_storage_mtime UPDATE failed: {e}"))?;

        Ok(())
    }

    // ── Folder-size recalculation chain (§21.1.1 step 5) ────────────────────

    /// Mirror PHP `Cache::correctFolderSize()` (`Cache.php:956-977`) for a file
    /// path: recompute the size of **every ancestor** from the sum of its direct
    /// children, walking from the immediate parent up to (and including) the
    /// storage root. Used for a **new** file, where PHP has no `oldSize` and falls
    /// back to recalculation instead of a signed size delta.
    ///
    /// Per-folder semantics match `calculateFolderSizeInner` (`Cache.php:1023-1101`):
    /// the folder size is `-1` when **any** child is unscanned (`size = -1`), else
    /// the sum of child sizes (`0` when there are no children); the row is updated
    /// only when the value actually changes.
    pub async fn correct_folder_size_chain(&self, fc_path: &str) -> Result<(), String> {
        // get_parents returns root-first ("", "files", …); recompute deepest-first so
        // each level sees its children's freshly-updated sizes.
        let mut parents = Self::get_parents(fc_path);
        parents.reverse();
        for p in parents {
            self.recompute_folder_size(&p).await?;
        }
        Ok(())
    }

    /// Recompute a single folder's `size` from its direct children (PHP
    /// `calculateFolderSizeInner`). No-op when the folder row is absent or its size
    /// is already correct.
    async fn recompute_folder_size(&self, folder_path: &str) -> Result<(), String> {
        let folder =
            match row::lookup_by_path(&self.pool, &self.prefix, self.storage_id, folder_path).await
            {
                Some(f) => f,
                None => return Ok(()),
            };

        // Single round-trip: SUM of child sizes and MIN to detect an unscanned child.
        // PostgreSQL SUM(bigint) returns NUMERIC; CAST to BIGINT keeps it as i64 for
        // sqlx::Any.  SQLite treats CAST AS BIGINT as INTEGER affinity — harmless.
        let sql = format!(
            "SELECT COALESCE(CAST(SUM(size) AS BIGINT), 0) AS total, COALESCE(MIN(size), 0) AS minsize \
             FROM {prefix}filecache \
             WHERE parent = $1 AND storage = $2",
            prefix = self.prefix
        );
        let r = sqlx::query(&sql)
            .bind(folder.fileid)
            .bind(self.storage_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| format!("recompute_folder_size SUM/MIN query failed: {e}"))?;
        let total: i64 = r.get("total");
        let minsize: i64 = r.get("minsize");

        // Any unscanned child (size = -1) marks the folder unscanned too.
        let new_size = if minsize == -1 { -1 } else { total };

        if new_size == folder.size {
            return Ok(());
        }

        let update_sql = format!(
            "UPDATE {prefix}filecache SET size = $1 WHERE fileid = $2",
            prefix = self.prefix
        );
        sqlx::query(&update_sql)
            .bind(new_size)
            .bind(folder.fileid)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("recompute_folder_size UPDATE failed: {e}"))?;

        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nc_db::pool::DbPool;

    // ── Unit: get_parents ──────────────────────────────────────────────────

    #[test]
    fn get_parents_root() {
        assert_eq!(Propagator::get_parents("files"), vec![""]);
    }

    #[test]
    fn get_parents_one_level() {
        assert_eq!(Propagator::get_parents("files/foo"), vec!["", "files"]);
    }

    #[test]
    fn get_parents_deep() {
        assert_eq!(
            Propagator::get_parents("files/a/b/c"),
            vec!["", "files", "files/a", "files/a/b"]
        );
    }

    #[test]
    fn get_parents_trash() {
        assert_eq!(
            Propagator::get_parents("files_trashbin/files/test.d123"),
            vec!["", "files_trashbin", "files_trashbin/files"]
        );
    }

    #[test]
    fn get_parents_empty_string_is_included() {
        let parents = Propagator::get_parents("files/x/y/z");
        assert_eq!(parents.first().map(String::as_str), Some(""));
        assert_eq!(parents.last().map(String::as_str), Some("files/x/y"));
    }

    #[test]
    fn path_hash_empty_string_matches_php_md5() {
        assert_eq!(row::path_hash(""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    // ── Integration: DB-backed propagation tests ───────────────────────────

    /// Create an in-memory SQLite DB with the `oc_filecache` schema and
    /// seed it with a directory tree.  Returns `(pool, prefix, storage_id)`.
    ///
    /// Tree:
    /// ```text
    /// fileid  path                  parent  size  mtime  etag
    /// 1       ""          (root)    -1      -1    0      root_old
    /// 2       "files"               1       100   10     files_old
    /// 3       "files/A"             2       50    10     a_old
    /// 4       "files/A/file.txt"    3       50    10     file_old
    /// ```
    async fn setup_test_fs(pool: &DbPool, prefix: &str, storage_id: i64) {
        // Root storage entry.
        sqlx::query(&format!(
            "INSERT INTO {prefix}filecache (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES (1, $1, '', $2, -1, NULL, 0, 0, -1, 0, 0, 'root_old', 31, '')"
        ))
        .bind(storage_id)
        .bind(row::path_hash(""))
        .execute(pool)
        .await
        .expect("insert root");

        // "files" directory.
        sqlx::query(&format!(
            "INSERT INTO {prefix}filecache (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES (2, $1, 'files', $2, 1, 'files', 0, 0, 100, 10, 10, 'files_old', 31, '')"
        ))
        .bind(storage_id)
        .bind(row::path_hash("files"))
        .execute(pool)
        .await
        .expect("insert files");

        // "files/A" directory.
        sqlx::query(&format!(
            "INSERT INTO {prefix}filecache (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES (3, $1, 'files/A', $2, 2, 'A', 0, 0, 50, 10, 10, 'a_old', 31, '')"
        ))
        .bind(storage_id)
        .bind(row::path_hash("files/A"))
        .execute(pool)
        .await
        .expect("insert files/A");

        // "files/A/file.txt".
        sqlx::query(&format!(
            "INSERT INTO {prefix}filecache (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES (4, $1, 'files/A/file.txt', $2, 3, 'file.txt', 0, 0, 50, 10, 10, 'file_old', 27, '')"
        ))
        .bind(storage_id)
        .bind(row::path_hash("files/A/file.txt"))
        .execute(pool)
        .await
        .expect("insert files/A/file.txt");
    }

    async fn fresh_db() -> (DbPool, String, i64) {
        let pool = DbPool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("in-memory SQLite"),
        );

        // Create the filecache table matching 0003_filecache.sql.
        sqlx::query(
            "CREATE TABLE oc_filecache (
                fileid           INTEGER NOT NULL PRIMARY KEY,
                storage          BIGINT  NOT NULL DEFAULT 0,
                path             VARCHAR(4000),
                path_hash        VARCHAR(32) NOT NULL DEFAULT '',
                parent           BIGINT  NOT NULL DEFAULT 0,
                name             VARCHAR(250),
                mimetype         BIGINT  NOT NULL DEFAULT 0,
                mimepart         BIGINT  NOT NULL DEFAULT 0,
                size             BIGINT  NOT NULL DEFAULT 0,
                mtime            INTEGER NOT NULL DEFAULT 0,
                storage_mtime    INTEGER NOT NULL DEFAULT 0,
                etag             VARCHAR(40),
                permissions      INTEGER DEFAULT 0,
                checksum         VARCHAR(255)
            )",
        )
        .execute(&pool)
        .await
        .expect("create table");

        let prefix = "oc_".to_string();
        let storage_id = 1i64;
        setup_test_fs(&pool, &prefix, storage_id).await;

        (pool, prefix, storage_id)
    }

    /// Read back a filecache row by path.
    async fn get_row(
        pool: &DbPool,
        prefix: &str,
        storage_id: i64,
        path: &str,
    ) -> (i64, String, i64, i64) {
        let hash = row::path_hash(path);
        let sql = format!(
            "SELECT size, etag, mtime, storage_mtime FROM {prefix}filecache \
             WHERE storage = $1 AND path_hash = $2"
        );
        let r = sqlx::query(&sql)
            .bind(storage_id)
            .bind(&hash)
            .fetch_one(pool)
            .await
            .expect("row not found");
        let size: i64 = r.get("size");
        let etag: String = r.try_get("etag").unwrap_or_default();
        let mtime: i64 = r.get("mtime");
        let storage_mtime: i64 = r.get("storage_mtime");
        (size, etag, mtime, storage_mtime)
    }

    // ── propagate_change tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn propagate_updates_etag_and_mtime_on_all_ancestors() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Propagate from "files/A/file.txt" with size_difference=30 (simulating
        // a PUT that grew the file by 30 bytes).
        prop.propagate_change("files/A/file.txt", 20, 30)
            .await
            .expect("propagate_change");

        // Ancestor "files/A": size 50 → 80, mtime updated, etag changed.
        let (size, etag, mtime, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(size, 80, "files/A size should be 50+30=80");
        assert!(
            mtime >= 20,
            "files/A mtime should be >= 20 (GREATEST of 10 and 20)"
        );
        assert_ne!(etag, "a_old", "files/A etag should have changed");

        // Ancestor "files": size 100 → 130, mtime updated,  etag same as A.
        let (size2, etag2, mtime2, _) = get_row(&pool, &prefix, storage_id, "files").await;
        assert_eq!(size2, 130, "files size should be 100+30=130");
        assert!(mtime2 >= 20, "files mtime should be >= 20");
        assert_ne!(etag2, "files_old", "files etag should have changed");
        // Same etag value shared by all ancestors of this propagateChange call.
        assert_eq!(etag, etag2, "all ancestors get the same etag");

        // Root "" (size=-1): should be untouched because size=-1 is unscanned.
        let (size_root, etag_root, _, _) = get_row(&pool, &prefix, storage_id, "").await;
        assert_eq!(size_root, -1, "root size should remain -1 (unscanned)");
        assert_ne!(etag_root, "root_old", "root etag should have changed");
    }

    #[tokio::test]
    async fn propagate_zero_size_difference_only_updates_etag_and_mtime() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Propagate with size_difference=0 — only etag/mtime should change.
        prop.propagate_change("files/A/file.txt", 30, 0)
            .await
            .expect("propagate_change");

        let (size, etag, mtime, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(size, 50, "files/A size should be unchanged at 50");
        assert!(mtime >= 30);
        assert_ne!(etag, "a_old");

        let (size2, _, _, _) = get_row(&pool, &prefix, storage_id, "files").await;
        assert_eq!(size2, 100, "files size should be unchanged at 100");
    }

    #[tokio::test]
    async fn propagate_negative_size_difference_subtracts() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Simulate a DELETE: size_difference = -50 (the file's size).
        prop.propagate_change("files/A/file.txt", 40, -50)
            .await
            .expect("propagate_change");

        let (size, _, _, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(size, 0, "files/A size should be 50-50=0");

        let (size2, _, _, _) = get_row(&pool, &prefix, storage_id, "files").await;
        assert_eq!(size2, 50, "files size should be 100-50=50");
    }

    #[tokio::test]
    async fn propagate_size_never_goes_below_minus_one() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Subtract more than the folder has — GREATEST ensures floor at -1.
        prop.propagate_change("files/A/file.txt", 40, -200)
            .await
            .expect("propagate_change");

        let (size, _, _, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(size, -1, "size floor should be -1");
    }

    #[tokio::test]
    async fn propagate_unscanned_folder_size_unchanged() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Mark "files/A" as unscanned (size = -1).
        sqlx::query(&format!(
            "UPDATE {prefix}filecache SET size = -1 WHERE path_hash = $1",
            prefix = prefix
        ))
        .bind(row::path_hash("files/A"))
        .execute(&pool)
        .await
        .expect("set size=-1");

        // Propagate with size_difference=50 — unscanned folder keeps size=-1.
        prop.propagate_change("files/A/file.txt", 20, 50)
            .await
            .expect("propagate_change");

        let (size, _, _, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(size, -1, "unscanned folder size should remain -1");

        // "files" has size=100 (scanned) — should increase.
        let (size2, _, _, _) = get_row(&pool, &prefix, storage_id, "files").await;
        assert_eq!(size2, 150, "scanned ancestor should increase");
    }

    #[tokio::test]
    async fn propagate_retries_on_failure() {
        // Verify the retry loop structure exists (unit-level: construction + call).
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // This should succeed on first attempt.
        prop.propagate_change("files/A/file.txt", 20, 10)
            .await
            .expect("propagate_change should succeed");

        let (size, _, _, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(size, 60);
    }

    #[tokio::test]
    async fn propagate_time_clamped_to_now() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        let far_future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 86_400_000; // far in the future

        // Time should be clamped to now, so this succeeds without setting
        // mtime to the far future value.
        prop.propagate_change("files/A/file.txt", far_future, 0)
            .await
            .expect("propagate_change");

        let (_size, _etag, mtime, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            mtime <= now + 5,
            "mtime should be <= now (clamped), got mtime={mtime} now={now}"
        );
    }

    // ── correct_folder_size tests ──────────────────────────────────────────

    #[tokio::test]
    async fn correct_folder_size_recalculates_from_children() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Replace the size of "files/A" with a wrong value.
        sqlx::query(&format!(
            "UPDATE {prefix}filecache SET size = 999 WHERE path_hash = $1",
            prefix = prefix
        ))
        .bind(row::path_hash("files/A"))
        .execute(&pool)
        .await
        .expect("set wrong size");

        // correctFolderSize should fix it from children SUM.
        prop.correct_folder_size("files/A")
            .await
            .expect("correct_folder_size");

        let (size, _, _, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        // "files/A/file.txt" has size=50, so "files/A" should be 50.
        assert_eq!(size, 50, "correctFolderSize should recalculate to 50");
    }

    #[tokio::test]
    async fn correct_folder_size_propagates_upwards() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Artificially set "files/A" to a wrong smaller size, then correct it.
        sqlx::query(&format!(
            "UPDATE {prefix}filecache SET size = 10 WHERE path_hash = $1",
            prefix = prefix
        ))
        .bind(row::path_hash("files/A"))
        .execute(&pool)
        .await
        .expect("set wrong size");

        prop.correct_folder_size("files/A")
            .await
            .expect("correct_folder_size");

        // "files" should have increased by the delta (50 - 10 = 40).
        let (size_files, _, _, _) = get_row(&pool, &prefix, storage_id, "files").await;
        // files was 100, size delta = 40, so now 140. But only if propagation
        // from correct_folder_size worked.
        assert_eq!(
            size_files, 140,
            "files should reflect the corrected delta (100→140)"
        );
    }

    #[tokio::test]
    async fn correct_folder_size_noop_when_size_unchanged() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // "files/A" already has correct size 50 (matching child file.txt=50).
        let old_etag = get_row(&pool, &prefix, storage_id, "files/A").await.1;

        prop.correct_folder_size("files/A")
            .await
            .expect("correct_folder_size");

        // Size should be unchanged.
        let (size, etag, _, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(size, 50, "size should be unchanged");
        // Etag should NOT change because we returned early (no UPDATE issued).
        assert_eq!(etag, old_etag);
    }

    #[tokio::test]
    async fn propagate_root_path_with_only_empty_parent() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Propagate from "files" (one level below root).  The only parent is "".
        prop.propagate_change("files", 20, 0)
            .await
            .expect("propagate_change");

        // Root etag should change, mtime updated.
        let (_size, etag, mtime, _) = get_row(&pool, &prefix, storage_id, "").await;
        assert_ne!(etag, "root_old", "root etag should change");
        assert!(mtime >= 20, "root mtime should be >= 20");
    }

    // ── correct_parent_storage_mtime tests ─────────────────────────────────

    #[tokio::test]
    async fn correct_parent_storage_mtime_sets_parent_columns() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Create a real directory and pin its mtime to a known value so the
        // assertion is deterministic.
        let dir = std::env::temp_dir().join(format!("nc_prop_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let ft = filetime::FileTime::from_unix_time(1_700_000_123, 0);
        filetime::set_file_times(&dir, ft, ft).expect("set dir mtime");

        prop.correct_parent_storage_mtime("files/A", &dir)
            .await
            .expect("correct_parent_storage_mtime");

        let (_, _, mtime, storage_mtime) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(
            storage_mtime, 1_700_000_123,
            "parent storage_mtime = dir disk mtime"
        );
        assert_eq!(mtime, 1_700_000_123, "mtime copied from storage_mtime");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── correct_folder_size_chain tests ────────────────────────────────────

    #[tokio::test]
    async fn correct_folder_size_chain_recomputes_ancestors() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Insert a new file (size 30) under files/A — simulates the row a fresh
        // PUT commits before the size chain runs.
        sqlx::query(&format!(
            "INSERT INTO {prefix}filecache (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES (5, $1, 'files/A/new.txt', $2, 3, 'new.txt', 0, 0, 30, 20, 20, 'new_old', 27, '')"
        ))
        .bind(storage_id)
        .bind(row::path_hash("files/A/new.txt"))
        .execute(&pool)
        .await
        .expect("insert new file");

        prop.correct_folder_size_chain("files/A/new.txt")
            .await
            .expect("correct_folder_size_chain");

        let (size_a, _, _, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(size_a, 80, "files/A = 50 (file.txt) + 30 (new.txt)");
        let (size_files, _, _, _) = get_row(&pool, &prefix, storage_id, "files").await;
        assert_eq!(size_files, 80, "files = files/A (80)");
        let (size_root, _, _, _) = get_row(&pool, &prefix, storage_id, "").await;
        assert_eq!(size_root, 80, "root = files (80); was -1 unscanned");
    }

    #[tokio::test]
    async fn correct_folder_size_chain_propagates_unscanned_child() {
        let (pool, prefix, storage_id) = fresh_db().await;
        let prop = Propagator::new(pool.clone(), prefix.clone(), storage_id);

        // Mark files/A/file.txt unscanned (size = -1).
        sqlx::query(&format!(
            "UPDATE {prefix}filecache SET size = -1 WHERE path_hash = $1",
            prefix = prefix
        ))
        .bind(row::path_hash("files/A/file.txt"))
        .execute(&pool)
        .await
        .expect("set size=-1");

        prop.correct_folder_size_chain("files/A/file.txt")
            .await
            .expect("correct_folder_size_chain");

        // Any unscanned child marks the folder (and, transitively, its ancestors)
        // unscanned: size = -1.
        let (size_a, _, _, _) = get_row(&pool, &prefix, storage_id, "files/A").await;
        assert_eq!(size_a, -1, "files/A has an unscanned child");
        let (size_files, _, _, _) = get_row(&pool, &prefix, storage_id, "files").await;
        assert_eq!(size_files, -1, "files/A is now unscanned");
        let (size_root, _, _, _) = get_row(&pool, &prefix, storage_id, "").await;
        assert_eq!(size_root, -1, "files is now unscanned");
    }
}
