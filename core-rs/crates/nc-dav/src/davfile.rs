//! `NcDavFile` — the `DavFile` trait implementation.
//!
//! Read and write operations use blocking I/O (`std::fs::File`) wrapped in
//! `tokio::task::block_in_place` / `spawn_blocking` so the async executor is
//! not starved during large transfers.
//!
//! The write path:
//! 1. `open()` creates a temp file next to the final target path.
//! 2. `write_bytes` / `write_buf` stream data into the temp file.
//! 3. `flush()` renames the temp to the final path **atomically**, then
//!    upserts the `oc_filecache` row with the new size, mtime, and etag.
//!
//! The read path:
//! 1. `open()` opens the existing file for reading.
//! 2. `read_bytes` / `seek` delegate to the `std::fs::File`.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use bytes::{Buf, Bytes};
use dav_server::fs::{DavFile, DavMetaData, FsError, FsFuture};
use futures::{FutureExt, future};
use tokio::task;

use crate::metadata::NcMetaData;
use crate::propagator::Propagator;
use nc_db::mime::SharedMimeCache;
use nc_db::pool::DbPool;

// ─── Preview cache path ─────────────────────────────────────────────────────

/// Build the PHP preview cache directory path for a given fileid.
///
/// PHP `LocalPreviewStorage::constructPath()` generates the path from the
/// MD5 of the fileid: 7 single-character subdirectories from the first 7
/// hex digits of `md5(fileid)`, followed by the fileid itself.
///
/// Path: `{datadir}/appdata_{instanceid}/preview/{c0}/{c1}/.../{c6}/{fileid}/`
pub(crate) fn preview_cache_dir(data_dir: &std::path::Path, instance_id: &str, fileid: i64) -> std::path::PathBuf {
    use md5::Digest;
    let digest = md5::Md5::digest(fileid.to_string().as_bytes());
    let hash = format!("{:x}", digest);
    let hash_dirs: String = hash
        .chars()
        .take(7)
        .flat_map(|c| [std::path::MAIN_SEPARATOR, c])
        .collect();
    data_dir
        .join(format!("appdata_{}", instance_id))
        .join("preview")
        .join(&hash_dirs[1..])
        .join(fileid.to_string())
}

// ─── Running hash ─────────────────────────────────────────────────────────────

/// Accumulates a hash over all bytes written during a PUT upload.
///
/// Compared against the client-supplied `OC-Checksum` header in `flush()`.
/// All three digest types re-export the same `digest::Digest` trait via
/// `md5::Digest` which covers them all (same version via workspace deps).
pub enum RunningHash {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    /// Adler-32 checksum (REQ §IMPL 4.4 — supported alongside MD5/SHA1/SHA256).
    Adler32(adler::Adler32),
    None,
}

impl RunningHash {
    /// Create a running hash from the value of an `OC-Checksum` header.
    ///
    /// Format: `ALGORITHM:hexhash` (e.g. `SHA1:abc123`, `MD5:deadbeef`).
    pub fn from_checksum_header(header: Option<&str>) -> Self {
        use md5::Digest as _;
        match header
            .and_then(|s| s.split(':').next())
            .map(str::to_uppercase)
            .as_deref()
        {
            Some("MD5")                  => RunningHash::Md5(md5::Md5::new()),
            Some("SHA1") | Some("SHA-1") => RunningHash::Sha1(sha1::Sha1::new()),
            Some("SHA256") | Some("SHA-256") => RunningHash::Sha256(sha2::Sha256::new()),
            Some("ADLER32") | Some("ADLER-32") => RunningHash::Adler32(adler::Adler32::new()),
            _ => RunningHash::None,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        use md5::Digest; // same trait for all three via digest re-export
        match self {
            RunningHash::Md5(h)     => h.update(data),
            RunningHash::Sha1(h)    => h.update(data),
            RunningHash::Sha256(h)  => h.update(data),
            RunningHash::Adler32(h) => h.write_slice(data),
            RunningHash::None       => {}
        }
    }

    pub fn finalize_hex(self) -> Option<String> {
        use md5::Digest;
        match self {
            RunningHash::Md5(h)     => Some(format!("{:x}", h.finalize())),
            RunningHash::Sha1(h)    => Some(format!("{:x}", h.finalize())),
            RunningHash::Sha256(h)  => Some(format!("{:x}", h.finalize())),
            // Adler-32 output is a u32 formatted as 8 lowercase hex digits;
            // matches PHP hash('adler32', ...) output.
            RunningHash::Adler32(h) => Some(format!("{:08x}", h.checksum())),
            RunningHash::None       => None,
        }
    }
}

// ─── Write context ────────────────────────────────────────────────────────────

/// Everything needed to commit a PUT to the database after the file has been
/// written to disk.
pub struct WriteCtx {
    pub temp_path:      PathBuf,
    pub final_path:     PathBuf,
    pub pool:           DbPool,
    pub prefix:         String,
    pub storage_id:     i64,
    /// Path within the storage — e.g. `files/Photos/img.jpg`.
    pub fc_path:        String,
    pub parent_id:      i64,
    pub uid:            String,
    pub mime_type_id:   i64,
    pub mimepart_id:    i64,
    /// `Some(fid)` when overwriting an existing entry; `None` for new files.
    pub initial_fileid: Option<i64>,
    /// Size of the existing file before overwrite (0 for new files).
    /// Used to compute `sizeDifference` for cache propagation (§9.2).
    pub old_size: i64,
    /// Mtime of the existing file before overwrite (0 for new files).
    /// Used to name the version file: `{path}.v{old_mtime}` (§9.4).
    pub old_mtime: i64,
    /// Mimetype ID of the existing file before overwrite (0 for new files).
    /// Used for the version's filecache row (§9.4).
    pub old_mimetype: i64,
    /// Permissions of the existing file before overwrite (27 for new files).
    /// Inherited by the version row so PHP-FPM versions PROPFIND shows correct owner metadata (§9.4).
    pub old_permissions: i32,
    /// Creation time of the existing file before overwrite (0 for new files).
    /// Inherited by the version row (§9.4).
    pub old_creation_time: i64,
    /// Upload time of the existing file before overwrite (0 for new files).
    /// Inherited by the version row (§9.4).
    pub old_upload_time: i64,
    /// Etag of the existing file before overwrite.  Inherited by the version
    /// row — PHP's `Cache::copyFromCache` clones the source row as-is, so the
    /// version file carries the old content's etag (live-verified).
    pub old_etag: Option<String>,
    /// `storage_mtime` of the existing file before overwrite.  The overwrite's
    /// etag is reused when the new disk mtime equals this (PHP's scanner keeps
    /// the etag on unchanged mtimes — Scanner.php:167-183), so the file and
    /// its version share the etag on same-second overwrites.
    pub old_storage_mtime: i64,
    /// Expected file size from `OpenOptions.size` (for validation).
    pub expected_size:  Option<u64>,
    /// Checksum from `OC-Checksum` header (e.g. `"SHA1:abc…"`).
    pub oc_checksum:    Option<String>,
    /// Running hash accumulating all written bytes for checksum validation.
    pub running_hash:   RunningHash,
    /// Client-supplied `X-OC-MTime` value (Unix seconds); `None` if absent.
    pub x_oc_mtime:     Option<i64>,
    /// Client-supplied `X-OC-CTime` value (Unix seconds); `None` if absent.
    pub x_oc_ctime:     Option<i64>,
    /// Written by `flush()` so `dav_handler` can inject response headers.
    pub write_result:   crate::SharedWriteResult,
    /// Set to `Some(PutErrorKind::…)` by `flush()` when it terminates early due
    /// to a known client error.  `dav_handler` reads this to rewrite the HTTP
    /// status (e.g. `GeneralFailure` 500 → `BadRequest` 400 for checksum mismatch).
    pub put_error:      crate::SharedPutError,
    /// Cache propagator — used in `flush()` to update parent ETag/mtime/size
    /// after the file is committed (§9.2).
    pub propagator:     Propagator,
    /// Data directory root — needed for version disk copy (§9.4).
    pub data_dir:       std::path::PathBuf,
    /// MIME cache — needed for version filecache row (§9.4).
    pub mime_cache:     SharedMimeCache,
    /// Nextcloud instance ID — needed to purge stale PHP preview caches.
    pub instance_id:    std::sync::Arc<String>,
}

impl std::fmt::Debug for WriteCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WriteCtx {{ fc_path: {:?} }}", self.fc_path)
    }
}

// ─── NcDavFile ────────────────────────────────────────────────────────────────

/// Open DAV file handle.
#[derive(Debug)]
pub struct NcDavFile {
    /// The OS-level file handle.  Stored as `Option` so it can be temporarily
    /// *taken out* into a blocking closure without lifetime issues.
    pub file:  Option<std::fs::File>,
    /// Metadata as seen at the time the file was opened.
    pub meta:  NcMetaData,
    /// Non-`None` only while in write mode.
    pub write: Option<WriteCtx>,
}

// ─── Blocking helper ──────────────────────────────────────────────────────────

async fn blocking<F, R>(func: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    match tokio::runtime::Handle::current().runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => task::block_in_place(func),
        _ => task::spawn_blocking(func).await.unwrap(),
    }
}

fn io_to_fs(e: io::Error) -> FsError {
    match e.kind() {
        io::ErrorKind::NotFound         => FsError::NotFound,
        io::ErrorKind::PermissionDenied => FsError::Forbidden,
        io::ErrorKind::AlreadyExists    => FsError::Exists,
        io::ErrorKind::OutOfMemory      => FsError::InsufficientStorage,
        _                               => FsError::GeneralFailure,
    }
}

// ─── DavFile impl ─────────────────────────────────────────────────────────────

impl DavFile for NcDavFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let m: Box<dyn DavMetaData> = Box::new(self.meta.clone());
        Box::pin(future::ok(m))
    }

    // ── Read ─────────────────────────────────────────────────────────────────

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        async move {
            let mut file = self.file.take().ok_or(FsError::GeneralFailure)?;
            let (res, f) = blocking(move || {
                let mut buf = vec![0u8; count];
                let r = file.read(&mut buf).map(|n| {
                    buf.truncate(n);
                    Bytes::from(buf)
                });
                (r, file)
            })
            .await;
            self.file = Some(f);
            res.map_err(io_to_fs)
        }
        .boxed()
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let mut file = self.file.take().ok_or(FsError::GeneralFailure)?;
            let (res, f) = blocking(move || {
                let r = file.seek(pos);
                (r, file)
            })
            .await;
            self.file = Some(f);
            res.map_err(io_to_fs)
        }
        .boxed()
    }

    // ── Write ─────────────────────────────────────────────────────────────────

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        async move {
            // Update running hash before moving buf into blocking closure.
            if let Some(ref mut ctx) = self.write {
                ctx.running_hash.update(&buf);
            }
            let mut file = self.file.take().ok_or(FsError::GeneralFailure)?;
            let (res, f) = blocking(move || {
                let r = file.write_all(&buf);
                (r, file)
            })
            .await;
            self.file = Some(f);
            res.map_err(io_to_fs)
        }
        .boxed()
    }

    fn write_buf(&'_ mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        async move {
            // Collect all bytes first so we can update the hash (buf is consumed
            // once moved into the blocking closure).
            let mut data = Vec::with_capacity(buf.remaining());
            while buf.has_remaining() {
                let chunk = buf.chunk();
                data.extend_from_slice(chunk);
                let n = chunk.len();
                buf.advance(n);
            }
            if let Some(ref mut ctx) = self.write {
                ctx.running_hash.update(&data);
            }
            let mut file = self.file.take().ok_or(FsError::GeneralFailure)?;
            let (res, f) = blocking(move || {
                let r = file.write_all(&data);
                (r, file)
            })
            .await;
            self.file = Some(f);
            res.map_err(io_to_fs)
        }
        .boxed()
    }

    // ── Flush / commit ────────────────────────────────────────────────────────

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        async move {
            let file = self.file.take().ok_or(FsError::GeneralFailure)?;

            let mut ctx = match self.write.take() {
                Some(c) => c,
                None => {
                    // Read-only: standard OS flush (no-op for read files).
                    let (res, f) = blocking(move || {
                        let r = (&file).flush();
                        (r, file)
                    })
                    .await;
                    self.file = Some(f);
                    return res.map_err(io_to_fs);
                }
            };

            // ── Checksum validation (REQ §13.1) ──────────────────────────────
            // Finalize the running hash and compare against the OC-Checksum
            // header value BEFORE we commit the rename.
            let running_hash = std::mem::replace(&mut ctx.running_hash, RunningHash::None);
            if let Some(ref expected) = ctx.oc_checksum {
                if let Some(computed_hex) = running_hash.finalize_hex() {
                    let expected_hash = expected.splitn(2, ':').nth(1).unwrap_or("");
                    if !computed_hex.eq_ignore_ascii_case(expected_hash) {
                        tracing::warn!(
                            expected = %expected,
                            computed = %computed_hex,
                            path = %ctx.fc_path,
                            "PUT checksum mismatch — returning 400"
                        );
                        // Remove the temp file before returning the error.
                        let temp = ctx.temp_path.clone();
                        let temp_display = temp.clone();
                        if let Err(e) = blocking(move || std::fs::remove_file(&temp)).await {
                            tracing::warn!(path = %temp_display.display(), error = %e, "Failed to remove temp file after checksum mismatch");
                        }
                        // Signal 400 Bad Request via the shared error channel.
                        // dav-server has no BadRequest FsError variant; we use
                        // GeneralFailure (→ 500) here and dav_handler rewrites
                        // it to 400 once it reads the channel.
                        if let Ok(mut e) = ctx.put_error.lock() {
                            *e = Some(crate::PutErrorKind::ChecksumMismatch);
                        }
                        return Err(FsError::GeneralFailure);
                    }
                }
            }

            // ── Mtime / ctime resolution ──────────────────────────────────────
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let use_mtime        = ctx.x_oc_mtime.unwrap_or(now);
            // PHP writes `creation_time` only when the client sent `X-OC-CTime`;
            // otherwise the column keeps its default `0` (finding #3 / phase-16.4,
            // resolved against `File.php:354-366` + `Cache::normalizeData`'s
            // array_filter drop of falsy extension fields).
            let use_creation_time = ctx.x_oc_ctime.unwrap_or(0);
            let mtime_accepted   = ctx.x_oc_mtime.is_some();
            let ctime_accepted   = ctx.x_oc_ctime.is_some();

            // §9.4: save a version BEFORE the rename overwrites the old file.
            // The old file still exists at ctx.final_path.
            if let Some(source_fileid) = ctx.initial_fileid {
                crate::versions::store_version(
                    &ctx.pool,
                    &ctx.prefix,
                    &ctx.data_dir,
                    &ctx.uid,
                    ctx.storage_id,
                    &ctx.mime_cache,
                    &ctx.fc_path,
                    &ctx.final_path,
                    ctx.old_size,
                    ctx.old_mtime,
                    ctx.old_mimetype,
                    ctx.old_permissions,
                    ctx.old_creation_time,
                    ctx.old_upload_time,
                    source_fileid,
                    ctx.old_etag.as_deref().unwrap_or(""),
                )
                .await;
            }

            // ── Blocking: flush OS buffer + get size ─────────────────────────
            let (sync_res, file_done) = blocking(move || {
                let mut f = file;
                if let Err(e) = f.flush() {
                    return (Err(e), f);
                }
                let size = match f.metadata() {
                    Ok(m)  => m.len(),
                    Err(e) => return (Err(e), f),
                };
                (Ok(size), f)
            })
            .await;

            drop(file_done);
            let size = sync_res.map_err(io_to_fs)?;

            // PHP's Quota wrapper rejects the write when the body size >= free
            // space (Quota.php:90-98 — `quota - used`); the DAV answers 507
            // InsufficientStorage and leaves no partial state (finding #24).
            if let Some(free) =
                crate::row::quota_free_space(&ctx.pool, &ctx.prefix, &ctx.uid, ctx.storage_id)
                    .await
            {
                if (size as i64) >= free {
                    let temp = ctx.temp_path.clone();
                    let temp_display = temp.clone();
                    if let Err(e) = blocking(move || std::fs::remove_file(&temp)).await {
                        tracing::warn!(path = %temp_display.display(), error = %e, "Failed to remove temp file after quota rejection");
                    }
                    return Err(FsError::InsufficientStorage);
                }
            }

            // ── Blocking: atomic rename ───────────────────────────────────────
            let temp_path  = ctx.temp_path.clone();
            let final_path = ctx.final_path.clone();
            blocking(move || std::fs::rename(&temp_path, &final_path))
                .await
                .map_err(io_to_fs)?;

            // ── Async: upsert oc_filecache ────────────────────────────────────
            let new_etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
            let checksum = ctx.oc_checksum.as_deref().unwrap_or("");
            let pool     = &ctx.pool;
            let prefix   = &ctx.prefix;
            let fileid;

            if let Some(fid) = ctx.initial_fileid {
                fileid = fid;
                // PHP's scanner reuses the existing etag when the file's disk
                // mtime is unchanged (Scanner.php:167-183 — a same-second
                // overwrite keeps the row's etag, and the version file — a
                // copy of this row — shares it).  Replicate: reuse the old
                // etag when the new disk mtime equals the old storage_mtime.
                let disk_mtime = std::fs::metadata(&ctx.final_path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(use_mtime);
                let etag_value = if ctx.old_storage_mtime != 0
                    && disk_mtime == ctx.old_storage_mtime
                {
                    ctx.old_etag.clone().unwrap_or_else(|| new_etag.clone())
                } else {
                    new_etag.clone()
                };
                let sql = format!(
                    "UPDATE {prefix}filecache \
                     SET size=$1, mtime=$2, storage_mtime=$3, etag=$4, checksum=$5 \
                     WHERE fileid=$6"
                );
                if let Err(e) = sqlx::query(&sql)
                    .bind(size as i64)
                    .bind(use_mtime)
                    .bind(use_mtime)
                    .bind(&etag_value)
                    .bind(checksum)
                    .bind(fid)
                    .execute(pool)
                    .await
                {
                    tracing::error!(error = %e, fileid = fid, "PUT: failed to update oc_filecache row");
                    return Err(FsError::GeneralFailure);
                }
            } else {
                let name = ctx.fc_path.rsplit('/').next().unwrap_or(&ctx.fc_path).to_string();
                let hash = crate::row::path_hash(&ctx.fc_path);
                let sql = format!(
                    "INSERT INTO {prefix}filecache \
                     (storage, path, path_hash, parent, name, mimetype, mimepart, \
                      size, mtime, storage_mtime, etag, permissions, checksum) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
                     RETURNING fileid"
                );

                // The database allocates the fileid atomically (sequence on
                // PostgreSQL, INTEGER PRIMARY KEY auto-increment on SQLite).
                // No retry loop needed — no MAX+1 race possible.
                let fid: i64 = match sqlx::query_scalar(&sql)
                    .bind(ctx.storage_id)
                    .bind(&ctx.fc_path)
                    .bind(&hash)
                    .bind(ctx.parent_id)
                    .bind(&name)
                    .bind(ctx.mime_type_id)
                    .bind(ctx.mimepart_id)
                    .bind(size as i64)
                    .bind(use_mtime)
                    .bind(use_mtime)
                    .bind(&new_etag)
                    .bind(27i32)
                    .bind(checksum)
                    .fetch_one(pool)
                    .await
                {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!(error = %e, fc_path = %ctx.fc_path, "PUT: failed to insert oc_filecache row");
                        return Err(FsError::GeneralFailure);
                    }
                };
                fileid = fid;
            }

            // ── Update in-memory metadata (PHASE-5.3) ────────────────────────
            // dav-server-rs calls `file.metadata()` after `flush()` to decide
            // what to put in the ETag response header.  `self.meta` was built
            // at `open()` time and is stale at this point; we refresh the
            // fields that changed so the response carries the correct values.
            self.meta.etag   = Some(new_etag.clone());
            self.meta.fileid = fileid;
            self.meta.size   = size as u64;
            self.meta.mtime  = use_mtime;

            // ── Propagate result to response header injector ──────────────────
            if let Ok(mut guard) = ctx.write_result.lock() {
                *guard = Some(crate::WriteResult {
                    fileid,
                    etag: new_etag,
                    mtime_accepted,
                    ctime_accepted,
                });
            }

            // ── Upsert oc_filecache_extended (REQ §4.4 / §21.1.4) ─────────────
            // `upload_time` is always the **request time** (`File.php:355`
            // `'upload_time' => time()`), independent of any `X-OC-MTime`.
            // `creation_time` is the client-supplied `X-OC-CTime` when present,
            // else the column default `0`.  On CONFLICT (existing row) only
            // `upload_time` is updated; `creation_time` is preserved — matching
            // PHP's `putFileInfo(['upload_time' => time()])`.
            let extended_sql = format!(
                "INSERT INTO {prefix}filecache_extended \
                 (fileid, creation_time, upload_time, metadata_etag) \
                 VALUES ($1, $2, $3, NULL) \
                 ON CONFLICT(fileid) DO UPDATE SET upload_time = excluded.upload_time",
                prefix = prefix
            );
            if let Err(e) = sqlx::query(&extended_sql)
                .bind(fileid)
                .bind(use_creation_time)
                .bind(now)
                .execute(pool)
                .await
            {
                tracing::warn!(fileid = fileid, error = %e, "PUT: failed to upsert oc_filecache_extended");
            }

            // §9.2 / §21.1.1: propagate to the parent chain, mirroring PHP
            // `Updater::update()` (Updater.php:68-94).  A **new** file has no
            // `oldSize`, so PHP recomputes ancestor sizes (`correctFolderSize`) and
            // then propagates ETag/mtime only (`sizeDifference = 0`).  An
            // **overwrite** knows `oldSize`, so PHP propagates the signed size delta.
            // In both cases `correctParentStorageMtime` runs before `propagateChange`.
            let is_new = ctx.initial_fileid.is_none();
            if is_new {
                if let Err(e) = ctx.propagator.correct_folder_size_chain(&ctx.fc_path).await {
                    tracing::warn!(path = %ctx.fc_path, error = %e, "PUT: folder-size chain failed");
                }
            }

            // Parent's `storage_mtime` (+`mtime`) ← parent directory disk mtime.
            let parent_fc_path = ctx
                .fc_path
                .rsplit_once('/')
                .map(|(p, _)| p.to_string())
                .unwrap_or_default();
            if let Some(parent_disk) = ctx.final_path.parent() {
                if let Err(e) = ctx
                    .propagator
                    .correct_parent_storage_mtime(&parent_fc_path, parent_disk)
                    .await
                {
                    tracing::warn!(
                        parent = %parent_fc_path,
                        error = %e,
                        "PUT: parent storage_mtime correction failed"
                    );
                }
            }

            let size_diff = if is_new { 0 } else { (size as i64) - ctx.old_size };
            if let Err(e) = ctx
                .propagator
                .propagate_change(&ctx.fc_path, use_mtime, size_diff)
                .await
            {
                tracing::warn!(path = %ctx.fc_path, error = %e, "PUT: propagation failed");
            }

            // Invalidate stale previews so PHP-FPM regenerates from the new
            // file content on next access.  Two layers must be cleared:
            //
            // 1. oc_previews DB rows (NC33+ preview metadata table).
            //    PHP's Generator::generatePreviews() queries this table first.
            //    If rows exist and the version matches PHP's cached file ETag
            //    (which is stale — PHP-FPM never learns Rust updated the DB),
            //    PHP serves the old preview.
            //
            // 2. Legacy preview files under appdata_/preview/.
            //    If oc_previews returns empty, PHP's migrateOldPreviews()
            //    re-imports files from this legacy cache right back into
            //    oc_previews — creating an infinite stale-cache cycle.
            //
            // Both must be cleared simultaneously to force a clean generation.
            // (Phase-11-compatible — oc_previews is the source of truth for Rust.)

            // Layer 1: DB metadata.
            let sql = format!(
                "DELETE FROM {prefix}previews WHERE file_id = $1",
                prefix = prefix
            );
            if let Err(e) = sqlx::query(&sql).bind(fileid).execute(pool).await {
                tracing::warn!(fileid = fileid, error = %e, "PUT: failed to delete stale oc_previews rows");
            }

            // Layer 2: Legacy preview files on disk.
            let preview_dir = preview_cache_dir(&ctx.data_dir, &ctx.instance_id, fileid);
            if let Err(e) = tokio::fs::remove_dir_all(&preview_dir).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        fileid = fileid,
                        preview_dir = %preview_dir.display(),
                        error = %e,
                        "Failed to purge legacy preview files"
                    );
                }
            }

            // §9.4: insert oc_files_versions for the file's current mtime so
            // PHP-FPM's versions PROPFIND has nc:version-author.
            // Matches PHP NodeWrittenEvent → post_write_hook → created()
            // → createVersionEntity() + VersionAuthorListener for every
            // successful write (new file or overwrite).
            crate::versions::insert_version_entity(
                &ctx.pool,
                &ctx.prefix,
                fileid,
                use_mtime,
                self.meta.size as i64,
                ctx.mime_type_id,
                &ctx.uid,
            )
            .await;

            // Finding #5 / phase-16.4: PHP's `previewgenerator` PostWriteListener
            // queues preview generation on every write (NodeWrittenEvent).  Reproduce
            // the side effect so the differential oracle finds the queue row.
            crate::preview_queue::queue_preview_generation(
                &ctx.pool,
                &ctx.prefix,
                &ctx.uid,
                fileid,
                now,
            )
            .await;

            // Finding #8: PHP materializes the user's `cache/` filecache row on
            // the first files access — the first PUT on a fresh instance shows
            // it (the delete path already materializes it in move_to_trash).
            crate::filesystem::ensure_lazy_dir_row(
                &ctx.pool,
                &ctx.prefix,
                ctx.storage_id,
                &ctx.mime_cache,
                "cache",
                now,
            )
            .await;

            Ok(())
        }
        .boxed()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_hash_md5() {
        let mut h = RunningHash::from_checksum_header(Some("MD5:ignored"));
        h.update(b"hello");
        h.update(b" world");
        let hex = h.finalize_hex().unwrap();
        // echo -n "hello world" | md5
        assert_eq!(hex, "5eb63bbbe01eeed093cb22bb8f5acdc3");
    }

    #[test]
    fn running_hash_sha1() {
        let mut h = RunningHash::from_checksum_header(Some("SHA1:ignored"));
        h.update(b"hello world");
        let hex = h.finalize_hex().unwrap();
        // echo -n "hello world" | sha1sum
        assert_eq!(hex, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");
    }

    #[test]
    fn running_hash_adler32() {
        let mut h = RunningHash::from_checksum_header(Some("ADLER32:ignored"));
        h.update(b"hello");
        let hex = h.finalize_hex().unwrap();
        // Adler-32 of b"hello": s1=533 (0x0215), s2=1580 (0x062C)
        // checksum = (s2 << 16) | s1 = 0x062C0215
        // PHP: hash('adler32', 'hello') == "062c0215"
        assert_eq!(hex, "062c0215");
    }

    #[test]
    fn running_hash_truly_unknown_algo_is_none() {
        let h = RunningHash::from_checksum_header(Some("XXXX:ignored"));
        assert!(h.finalize_hex().is_none());
    }

    #[test]
    fn running_hash_no_header_is_none() {
        let h = RunningHash::from_checksum_header(None);
        assert!(h.finalize_hex().is_none());
    }
}
