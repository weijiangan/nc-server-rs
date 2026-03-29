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
use nc_db::pool::DbPool;

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
                        // Remove the temp file before returning the error.
                        let temp = ctx.temp_path.clone();
                        let _ = blocking(move || std::fs::remove_file(&temp)).await;
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
            let use_creation_time = ctx.x_oc_ctime.unwrap_or(now);
            let mtime_accepted   = ctx.x_oc_mtime.is_some();
            let ctime_accepted   = ctx.x_oc_ctime.is_some();

            // ── Blocking: flush OS buffer + get size + atomic rename ──────────
            let temp_path  = ctx.temp_path.clone();
            let final_path = ctx.final_path.clone();

            let (sync_res, file_done) = blocking(move || {
                let mut f = file;
                if let Err(e) = f.flush() {
                    return (Err(e), f);
                }
                let size = match f.metadata() {
                    Ok(m)  => m.len(),
                    Err(e) => return (Err(e), f),
                };
                if let Err(e) = std::fs::rename(&temp_path, &final_path) {
                    return (Err(e), f);
                }
                (Ok(size), f)
            })
            .await;

            drop(file_done);
            let size = sync_res.map_err(io_to_fs)?;

            // ── Async: upsert oc_filecache ────────────────────────────────────
            let new_etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
            let checksum = ctx.oc_checksum.as_deref().unwrap_or("");
            let pool     = &ctx.pool;
            let prefix   = &ctx.prefix;
            let fileid;

            if let Some(fid) = ctx.initial_fileid {
                fileid = fid;
                let sql = format!(
                    "UPDATE {prefix}filecache \
                     SET size=?, mtime=?, storage_mtime=?, etag=?, checksum=?, upload_time=? \
                     WHERE fileid=?"
                );
                let _ = sqlx::query(&sql)
                    .bind(size as i64)
                    .bind(use_mtime)
                    .bind(use_mtime)
                    .bind(&new_etag)
                    .bind(checksum)
                    .bind(now)
                    .bind(fid)
                    .execute(pool)
                    .await;
            } else {
                let fid = crate::row::next_fileid(pool, prefix)
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                fileid = fid;

                let name = ctx.fc_path.rsplit('/').next().unwrap_or(&ctx.fc_path).to_string();
                let hash = crate::row::path_hash(&ctx.fc_path);

                let sql = format!(
                    "INSERT INTO {prefix}filecache \
                     (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
                      size, mtime, storage_mtime, etag, permissions, checksum, creation_time, upload_time) \
                     VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
                );
                let _ = sqlx::query(&sql)
                    .bind(fid)
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
                    .bind(use_creation_time)
                    .bind(now)
                    .execute(pool)
                    .await;
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

            // ── Upsert oc_filecache_extended (REQ §4.4) ──────────────────────
            // Always update upload_time = use_mtime (the effective mtime of this
            // upload).  On INSERT (new row): also set creation_time from the
            // client-supplied X-OC-CTime or current time.  On CONFLICT (existing
            // row): only upload_time is updated; creation_time is preserved.
            let extended_sql = format!(
                "INSERT INTO {prefix}filecache_extended \
                 (fileid, creation_time, upload_time, metadata_etag) \
                 VALUES (?, ?, ?, NULL) \
                 ON CONFLICT(fileid) DO UPDATE SET upload_time = excluded.upload_time",
                prefix = prefix
            );
            let _ = sqlx::query(&extended_sql)
                .bind(fileid)
                .bind(use_creation_time)
                .bind(use_mtime)
                .execute(pool)
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
