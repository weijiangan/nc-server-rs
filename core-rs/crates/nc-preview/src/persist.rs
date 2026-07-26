//! Persisting generated previews + overwrite invalidation (Phase 11.5).
//!
//! Three responsibilities, all verified against the PHP reference:
//!
//! - **Insert** a generated preview row with a client-side **snowflake** id and
//!   PHP-exact column parity, so a subsequent **PHP** request finds and serves the
//!   Rust-written row (`Generator::generateProviderPreview` `:392-405` /
//!   `generatePreview` `:550-563`, `PreviewMapper::insert` `:49-68`).  The unique
//!   index `(file_id, width, height, mimetype_id, cropped, version_id)` guards the
//!   cross-writer race (PHP ↔ Rust, Rust ↔ Rust): on conflict we re-fetch and serve
//!   the winner (`Generator::getMaxPreview:338-345`).
//! - **Write** the bytes to the md5-sharded appdata path (`LocalPreviewStorage`
//!   `:81-83`) — via write-then-rename so a concurrent reader never sees a partial
//!   file (an intentional, strict improvement over PHP's direct `file_put_contents`,
//!   producing an identical final file).
//! - **Invalidate** on content overwrite (Watcher parity — correctness-critical):
//!   delete all of a file's preview rows + bytes (`Watcher::postWrite` `:36-61`).
//!   There is no mtime/etag comparison at read — deletion *is* the invalidation.
//!
//! Scope: **local-disk, un-versioned** files (`version_id = -1`); object-store
//! locations and versioned previews fall back to PHP-FPM (consistent with the rest
//! of Phase 11's local-storage assumption).

use crate::snowflake::SnowflakeGenerator;
use crate::store::{self, PreviewRow};
use nc_db::mime::SharedMimeCache;
use nc_db::pool::DbPool;
use std::path::Path;

/// The fields of a preview row to insert (id is generated; `encrypted` is always
/// `false`; `old_file_id`/`location_id` are NULL on local disk).
#[derive(Debug, Clone)]
pub struct NewPreview {
    pub file_id: i64,
    /// The mount's numeric storage id (`File::getMountPoint()->getNumericStorageId()`).
    pub storage_id: i64,
    /// Actual produced width (max preview: Imaginary's real dims; derived: the
    /// requested bucketed dims — PHP `generatePreview` stores the requested dims).
    pub width: u32,
    pub height: u32,
    /// **Output** mimetype id (the preview image's mime), via `oc_mimetypes`.
    pub mimetype_id: i32,
    /// Source file's mimetype id.
    pub source_mimetype_id: i32,
    /// `true` only for the single max preview.
    pub max: bool,
    pub cropped: bool,
    /// The **source file's etag at generation** (`Generator.php:404`) — not the
    /// preview bytes' etag.  Stored into `CHAR(40)`; Postgres blank-pads it (and the
    /// serve path re-pads on read — see `store::load_preview_rows`).
    pub etag: String,
    /// **Generation** timestamp (`Generator.php:405`) — not the file's mtime.
    pub mtime: i64,
    /// Byte size of the stored preview.
    pub size: i64,
    /// `-1` for un-versioned (local disk).
    pub version_id: i64,
}

impl NewPreview {
    /// Build the [`PreviewRow`] this insert produced (the row as subsequently read).
    fn to_row(&self, id: i64) -> PreviewRow {
        PreviewRow {
            id,
            file_id: self.file_id,
            storage_id: self.storage_id,
            width: self.width,
            height: self.height,
            mimetype_id: self.mimetype_id,
            source_mimetype_id: self.source_mimetype_id,
            mtime: self.mtime,
            size: self.size,
            max: self.max,
            cropped: self.cropped,
            encrypted: false,
            etag: self.etag.clone(),
            version_id: self.version_id,
        }
    }

    /// Whether an existing row matches this preview's unique key.
    fn matches_row(&self, r: &PreviewRow) -> bool {
        r.width == self.width
            && r.height == self.height
            && r.mimetype_id == self.mimetype_id
            && r.cropped == self.cropped
            && r.version_id == self.version_id
    }
}

/// A persistence failure.
#[derive(thiserror::Error, Debug)]
pub enum PersistError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("preview byte write failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("insert lost the unique-index race but the winning row could not be re-fetched")]
    ConflictLost,
}

/// Insert a preview row with a fresh snowflake id and PHP-exact column parity.
///
/// On a unique-index conflict `(file_id, width, height, mimetype_id, cropped,
/// version_id)` — a race with PHP or another Rust task — re-fetch and return the
/// winning row instead of failing (PHP `Generator::getMaxPreview:338-345`).  The
/// bytes at the deterministic path are a valid preview regardless of which writer
/// produced them, so the winner's row serves correctly.
pub async fn insert_preview(
    pool: &DbPool,
    prefix: &str,
    p: &NewPreview,
    id: i64,
) -> Result<PreviewRow, PersistError> {
    // `encrypted` is always false; `old_file_id`/`location_id` are NULL (local disk).
    // Integer columns are bound at their exact SQL types (`integer` → i32, `bigint`
    // → i64) so Postgres accepts them without coercion.
    let sql = format!(
        "INSERT INTO {prefix}previews \
         (id, file_id, storage_id, width, height, mimetype_id, source_mimetype_id, \
          max, cropped, encrypted, etag, mtime, size, version_id, old_file_id, location_id) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,NULL,NULL) \
         ON CONFLICT (file_id, width, height, mimetype_id, cropped, version_id) DO NOTHING \
         RETURNING id"
    );
    let inserted = sqlx::query(&sql)
        .bind(id)
        .bind(p.file_id)
        .bind(p.storage_id)
        .bind(p.width as i32)
        .bind(p.height as i32)
        .bind(p.mimetype_id)
        .bind(p.source_mimetype_id)
        .bind(p.max)
        .bind(p.cropped)
        .bind(false) // encrypted
        .bind(&p.etag)
        .bind(p.mtime as i32)
        .bind(p.size as i32)
        .bind(p.version_id)
        .fetch_optional(pool)
        .await?;

    if inserted.is_some() {
        // We won the race — this is the row we inserted.
        Ok(p.to_row(id))
    } else {
        // Conflict — return the existing row for this key.
        fetch_by_key(pool, prefix, p).await
    }
}

/// Fetch the existing row matching a preview's unique key (the conflict re-fetch).
async fn fetch_by_key(
    pool: &DbPool,
    prefix: &str,
    p: &NewPreview,
) -> Result<PreviewRow, PersistError> {
    let rows = store::load_preview_rows(pool, prefix, p.file_id).await;
    rows.into_iter()
        .find(|r| p.matches_row(r))
        .ok_or(PersistError::ConflictLost)
}

/// Write preview bytes to the md5-sharded appdata path (`LocalPreviewStorage::
/// constructPath`), creating parent directories.  Uses **write-then-rename** so a
/// concurrent reader (PHP or Rust) never observes a partial file — an intentional
/// improvement over PHP's direct `file_put_contents`; the final file is identical.
/// Returns the byte count written.
pub async fn write_preview_bytes(
    datadir: &Path,
    instanceid: &str,
    file_id: i64,
    name: &str,
    bytes: &[u8],
    unique: i64,
) -> Result<u64, std::io::Error> {
    let path = store::preview_byte_path(datadir, instanceid, file_id, name);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // A per-write temp name (the snowflake id keeps it unique) in the SAME directory,
    // so the rename is atomic on the same filesystem.
    let tmp = path.with_file_name(format!("{name}.tmp{unique}"));
    tokio::fs::write(&tmp, bytes).await?;
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        // Best-effort cleanup of the temp file on rename failure.
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(bytes.len() as u64)
}

/// Generate the next snowflake id (convenience wrapper for callers that hold the
/// shared generator).
pub fn next_id(snowflake: &SnowflakeGenerator) -> i64 {
    snowflake.next_id()
}

/// **Invalidate all previews for a file** (Watcher parity, `Watcher::postWrite`).
///
/// Deletes every `oc_previews` row for `file_id`, then unlinks each preview's bytes
/// (best-effort).  Rows are deleted **first** so no new request can start serving a
/// row whose bytes are about to disappear.  Byte-deletion failures are logged at
/// `warn!` and otherwise left to PHP's hourly orphan sweep (CLAUDE.md hygiene rule 1:
/// a silent failure here is invisible to the caller).  Returns the number of rows
/// deleted.
///
/// Called on the Rust PUT-overwrite path (content writes).  **Not** on MOVE/COPY
/// (same content — PHP doesn't invalidate) nor on a pure mtime touch (PHP fires
/// `postTouch`, not `postWrite`, so it does not invalidate either).
pub async fn invalidate_previews(
    pool: &DbPool,
    prefix: &str,
    datadir: &Path,
    instanceid: &str,
    file_id: i64,
    mime_cache: &SharedMimeCache,
) -> usize {
    if file_id <= 0 {
        return 0;
    }
    let rows = store::load_preview_rows(pool, prefix, file_id).await;
    if rows.is_empty() {
        return 0;
    }

    // Delete the rows first (no new hits can start serving them).
    let sql = format!("DELETE FROM {prefix}previews WHERE file_id = $1");
    match sqlx::query(&sql).bind(file_id).execute(pool).await {
        Ok(res) => {
            let deleted = res.rows_affected() as usize;
            tracing::debug!(file_id, deleted, "invalidated preview rows on overwrite");
        }
        Err(e) => {
            // Leave the bytes in place — the rows still reference them, so the
            // previews keep serving.  Surface loudly (CLAUDE.md hygiene rule 1).
            tracing::error!(error = %e, file_id, "invalidate: failed to delete preview rows");
            return 0;
        }
    }

    // Resolve the byte paths to remove while holding the cache lock — crucially, with
    // NO await under the lock (a `std::sync` guard held across an await can stall the
    // executor if another task needs the write lock).
    let paths: Vec<std::path::PathBuf> = {
        let cache = mime_cache.read().expect("mime cache lock");
        rows.iter()
            .filter_map(|row| {
                let out_mime = match cache.get_name(row.mimetype_id as i64) {
                    Some(m) => m,
                    None => {
                        tracing::warn!(
                            file_id,
                            mimetype_id = row.mimetype_id,
                            "invalidate: unknown output mimetype id; byte left for orphan sweep"
                        );
                        return None;
                    }
                };
                let name = store::preview_name(
                    row.version_id, row.width, row.height, row.cropped, row.max, out_mime,
                );
                Some(store::preview_byte_path(datadir, instanceid, file_id, &name))
            })
            .collect()
    };

    // Guard dropped — now unlink (best-effort).
    let mut removed = 0usize;
    for path in &paths {
        match tokio::fs::remove_file(path).await {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone (PHP deleted it, or it was never written) — fine.
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    file_id,
                    path = %path.display(),
                    "invalidate: failed to unlink preview bytes (orphan sweep will clean up)"
                );
            }
        }
    }
    tracing::debug!(file_id, removed, "unlinked preview bytes on overwrite");
    rows.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::find_max;

    async fn sqlite_pool() -> DbPool {
        sqlx::any::install_default_drivers();
        // Single connection: an in-memory SQLite DB is per-connection.
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // oc_previews per REQ §9.10 (boolean columns as INTEGER for SQLite's Any driver).
        sqlx::query(
            "CREATE TABLE oc_previews (
                id                 BIGINT  NOT NULL PRIMARY KEY,
                file_id            BIGINT  NOT NULL,
                storage_id         BIGINT  NOT NULL,
                old_file_id        BIGINT,
                location_id        BIGINT,
                width              INTEGER NOT NULL,
                height             INTEGER NOT NULL,
                mimetype_id        INTEGER NOT NULL,
                source_mimetype_id INTEGER NOT NULL,
                max                INTEGER NOT NULL DEFAULT 0,
                cropped            INTEGER NOT NULL DEFAULT 0,
                encrypted          INTEGER NOT NULL DEFAULT 0,
                etag               VARCHAR(40) NOT NULL DEFAULT '',
                mtime              INTEGER NOT NULL DEFAULT 0,
                size               INTEGER NOT NULL DEFAULT 0,
                version_id         BIGINT  NOT NULL DEFAULT -1
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX previews_file_uniq_idx ON oc_previews \
             (file_id, width, height, mimetype_id, cropped, version_id)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // oc_mimetypes so invalidation can resolve output-mime ids → names.
        sqlx::query("CREATE TABLE oc_mimetypes (id INTEGER PRIMARY KEY, mimetype VARCHAR(255) NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        for (id, mime) in [
            (5i64, "image/jpeg"),
            (6i64, "image/png"),
            (9i64, "application/octet-stream"),
        ] {
            sqlx::query("INSERT INTO oc_mimetypes (id, mimetype) VALUES ($1, $2)")
                .bind(id)
                .bind(mime)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    /// Build the mime cache from the test DB (id 5 → image/jpeg, etc.).
    async fn mime_cache(pool: &DbPool) -> SharedMimeCache {
        nc_db::mime::load_mime_cache(pool, "oc_").await.unwrap()
    }

    fn newp(w: u32, h: u32, max: bool, cropped: bool, mime_id: i32) -> NewPreview {
        NewPreview {
            file_id: 123,
            storage_id: 1,
            width: w,
            height: h,
            mimetype_id: mime_id,
            source_mimetype_id: 9,
            max,
            cropped,
            etag: "srcetag".to_string(),
            mtime: 1_700_000_000,
            size: 4096,
            version_id: -1,
        }
    }

    // ── insert + column parity ─────────────────────────────────────────────

    #[tokio::test]
    async fn insert_writes_php_parity_row() {
        let pool = sqlite_pool().await;
        let p = newp(1351, 901, true, false, 5);
        let row = insert_preview(&pool, "oc_", &p, 1001).await.unwrap();

        assert_eq!(row.id, 1001);
        assert_eq!((row.width, row.height), (1351, 901));
        assert!(row.max && !row.cropped && !row.encrypted);
        assert_eq!(row.mimetype_id, 5);
        assert_eq!(row.source_mimetype_id, 9);
        assert_eq!(row.etag, "srcetag"); // source etag at generation
        assert_eq!(row.mtime, 1_700_000_000); // generation timestamp
        assert_eq!(row.version_id, -1); // un-versioned (local disk)

        // Round-trips through the serve-path reader.
        let rows = store::load_preview_rows(&pool, "oc_", 123).await;
        assert_eq!(rows.len(), 1);
        let max = find_max(&rows, -1).unwrap();
        assert_eq!(max.id, 1001);
    }

    // ── unique-index collision → re-fetch the winner ───────────────────────

    #[tokio::test]
    async fn insert_conflict_refetches_existing_row() {
        let pool = sqlite_pool().await;
        let p = newp(256, 256, false, true, 5);
        // First writer wins with id 2001.
        let first = insert_preview(&pool, "oc_", &p, 2001).await.unwrap();
        assert_eq!(first.id, 2001);
        // Second writer (different id, same key) loses the race → gets the winner.
        let second = insert_preview(&pool, "oc_", &p, 2002).await.unwrap();
        assert_eq!(second.id, 2001, "conflict must return the existing row");
        // Still exactly one row.
        assert_eq!(store::load_preview_rows(&pool, "oc_", 123).await.len(), 1);
    }

    #[tokio::test]
    async fn distinct_keys_insert_independently() {
        let pool = sqlite_pool().await;
        insert_preview(&pool, "oc_", &newp(256, 256, false, true, 5), 1).await.unwrap();
        insert_preview(&pool, "oc_", &newp(512, 512, false, false, 5), 2).await.unwrap();
        // Same dims but different output mime → distinct key.
        insert_preview(&pool, "oc_", &newp(256, 256, false, true, 7), 3).await.unwrap();
        assert_eq!(store::load_preview_rows(&pool, "oc_", 123).await.len(), 3);
    }

    // ── byte write (write-then-rename) ─────────────────────────────────────

    #[tokio::test]
    async fn write_bytes_creates_sharded_path_no_temp_left() {
        let dir = tempdir();
        let n = write_preview_bytes(&dir, "oc1", 123, "256-256-crop.png", b"PNGDATA", 42).await.unwrap();
        assert_eq!(n, 7);
        let path = store::preview_byte_path(&dir, "oc1", 123, "256-256-crop.png");
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"PNGDATA");
        // No leftover temp file in the directory.
        let mut entries = tokio::fs::read_dir(path.parent().unwrap()).await.unwrap();
        let mut names = Vec::new();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, vec!["256-256-crop.png".to_string()], "temp file must be renamed away");
    }

    // ── invalidation (Watcher parity) ──────────────────────────────────────

    #[tokio::test]
    async fn invalidate_deletes_rows_and_bytes() {
        let pool = sqlite_pool().await;
        let dir = tempdir();
        // Two previews for file 123 (both output jpeg, id 5).
        let max = newp(1351, 901, true, false, 5);
        let derived = newp(256, 256, false, true, 5);
        insert_preview(&pool, "oc_", &max, 1).await.unwrap();
        insert_preview(&pool, "oc_", &derived, 2).await.unwrap();
        // Write both byte files.
        let max_name = store::preview_name(-1, 1351, 901, false, true, "image/jpeg");
        let der_name = store::preview_name(-1, 256, 256, true, false, "image/jpeg");
        write_preview_bytes(&dir, "oc1", 123, &max_name, b"MAX", 1).await.unwrap();
        write_preview_bytes(&dir, "oc1", 123, &der_name, b"DER", 2).await.unwrap();

        let cache = mime_cache(&pool).await;
        let deleted = invalidate_previews(&pool, "oc_", &dir, "oc1", 123, &cache).await;
        assert_eq!(deleted, 2);

        // Rows gone.
        assert!(store::load_preview_rows(&pool, "oc_", 123).await.is_empty());
        // Bytes gone.
        assert!(tokio::fs::read(store::preview_byte_path(&dir, "oc1", 123, &max_name)).await.is_err());
        assert!(tokio::fs::read(store::preview_byte_path(&dir, "oc1", 123, &der_name)).await.is_err());
    }

    #[tokio::test]
    async fn invalidate_no_rows_is_a_noop() {
        let pool = sqlite_pool().await;
        let dir = tempdir();
        let cache = mime_cache(&pool).await;
        assert_eq!(invalidate_previews(&pool, "oc_", &dir, "oc1", 999, &cache).await, 0);
        // A non-positive file id is never touched.
        assert_eq!(invalidate_previews(&pool, "oc_", &dir, "oc1", 0, &cache).await, 0);
    }

    /// A unique temp directory under the system temp root.
    fn tempdir() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!(
            "nc_preview_test_{}_{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
