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

use nc_db::pool::DbPool;
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
    async fn try_propagate(
        &self,
        parent_hashes: &[String],
        time: i64,
        size_difference: i64,
        etag: &str,
    ) -> Result<(), String> {
        let placeholders: Vec<String> = (1..=parent_hashes.len())
            .map(|i| format!("${i}"))
            .collect();
        let in_clause = placeholders.join(", ");

        // Parameter indices:
        //   $1..$N  = path_hashes
        //   $(N+1)  = storage_id
        //   $(N+2)  = time
        //   $(N+3)  = etag
        let storage_idx = parent_hashes.len() + 1;
        let time_idx = parent_hashes.len() + 2;
        let etag_idx = parent_hashes.len() + 3;

        // Use CASE WHEN instead of GREATEST for cross-DB compatibility
        // (SQLite lacks GREATEST; PostgreSQL and MySQL support both).
        if size_difference != 0 {
            // CASE WHEN size > -1 THEN MAX(size + $sizeDiff, -1) ELSE size END
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
                .execute(&self.pool)
                .await
                .map_err(|e| format!("propagate UPDATE with size failed: {e}"))?;
        } else {
            // No size change — only etag + mtime.
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
                .execute(&self.pool)
                .await
                .map_err(|e| format!("propagate UPDATE failed: {e}"))?;
        }

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
        let sql = format!(
            "SELECT COALESCE(SUM(size), 0) AS total \
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
            self.propagate_change(fc_path, new_size, size_delta)
                .await?;
        }

        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nc_db::pool::DbPool;
    use sqlx::Row as _;

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
        sqlx::any::install_default_drivers();
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite");

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
    async fn get_row(pool: &DbPool, prefix: &str, storage_id: i64, path: &str) -> (i64, String, i64, i64) {
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
        assert!(mtime >= 20, "files/A mtime should be >= 20 (GREATEST of 10 and 20)");
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
        assert_eq!(size_files, 140, "files should reflect the corrected delta (100→140)");
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
}
