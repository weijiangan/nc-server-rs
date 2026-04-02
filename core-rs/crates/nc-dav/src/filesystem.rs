//! `NcFileSystem` — the `DavFileSystem` trait implementation.
//!
//! One instance is created **per request** with the authenticated user's `uid`
//! and pre-resolved `storage_id`.  The instance is cheap to create (all
//! expensive state is held in `NcDavState` behind `Arc`s).
//!
//! ## Path mapping
//!
//! WebDAV paths are relative to the DAV root (after the prefix is stripped by
//! `DavConfig::strip_prefix`).  They map to `oc_filecache` paths as:
//!
//! ```text
//! DAV path          oc_filecache path        Disk path
//! /                 files                    {data_dir}/{uid}/files
//! /Photos/img.jpg   files/Photos/img.jpg     {data_dir}/{uid}/files/Photos/img.jpg
//! ```

use std::io;
use std::path::PathBuf;

use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, DavProp, FsError, FsFuture, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures::{future, stream, FutureExt};
use sqlx::Row;
use tokio::task;
use tracing::warn;

use crate::{
    davfile::{NcDavFile, WriteCtx},
    metadata::{NcDirEntry, NcMetaData},
    row::{self, dav_to_fc_path, disk_path},
    NcDavState,
};

// ─── NcFileSystem ─────────────────────────────────────────────────────────────

/// Per-request DAV filesystem bound to one user's home storage.
#[derive(Clone)]
pub struct NcFileSystem {
    pub(crate) state: NcDavState,
    pub(crate) uid: String,
    pub(crate) storage_id: i64,
    /// `X-OC-MTime` header value parsed from the request (Unix seconds).
    pub(crate) x_oc_mtime: Option<i64>,
    /// `X-OC-CTime` header value parsed from the request (Unix seconds).
    pub(crate) x_oc_ctime: Option<i64>,
    /// Channel through which `flush()` returns write metadata for response headers.
    pub(crate) write_result: crate::SharedWriteResult,
    /// Channel through which `flush()` signals known PUT errors (checksum
    /// mismatch, etc.) so `dav_handler` can rewrite the HTTP status code.
    pub(crate) put_error: crate::SharedPutError,
}

impl NcFileSystem {
    pub fn new(
        state: NcDavState,
        uid: String,
        storage_id: i64,
        x_oc_mtime: Option<i64>,
        x_oc_ctime: Option<i64>,
        write_result: crate::SharedWriteResult,
        put_error: crate::SharedPutError,
    ) -> Self {
        NcFileSystem {
            state,
            uid,
            storage_id,
            x_oc_mtime,
            x_oc_ctime,
            write_result,
            put_error,
        }
    }

    /// Convert a `DavPath` to an `oc_filecache` path.
    fn to_fc_path(&self, path: &dav_server::davpath::DavPath) -> String {
        let raw = String::from_utf8_lossy(path.as_bytes()).into_owned();
        dav_to_fc_path(&raw)
    }

    /// Resolve the on-disk path for a filecache path.
    fn disk_path(&self, fc_path: &str) -> PathBuf {
        let data_dir = self.state.data_directory.as_path();
        disk_path(data_dir, &self.uid, fc_path)
    }

    /// Load `NcMetaData` for any filecache path, including extended times.
    async fn load_meta(&self, fc_path: &str) -> Option<NcMetaData> {
        let row = row::lookup_by_path(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            fc_path,
        )
        .await?;

        let mime_type = {
            let cache = self.state.mime_cache.read().expect("mime cache lock");
            cache
                .get_name(row.mimetype)
                .unwrap_or("application/octet-stream")
                .to_string()
        };

        let ext = row::get_extended(&self.state.pool, &self.state.table_prefix, row.fileid).await;
        let mut meta = NcMetaData::from_row(&row, mime_type, ext.metadata_etag.clone());
        meta.apply_extended(ext.creation_time, ext.upload_time, ext.metadata_etag);
        Some(meta)
    }
}

// ─── blocking helper ──────────────────────────────────────────────────────────

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
        io::ErrorKind::NotFound => FsError::NotFound,
        io::ErrorKind::PermissionDenied => FsError::Forbidden,
        io::ErrorKind::AlreadyExists => FsError::Exists,
        _ => FsError::GeneralFailure,
    }
}

// ─── DavFileSystem impl ────────────────────────────────────────────────────────

impl DavFileSystem for NcFileSystem {
    // ── metadata ─────────────────────────────────────────────────────────────

    fn metadata<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let fc_path = self.to_fc_path(path);
            let meta = self.load_meta(&fc_path).await.ok_or(FsError::NotFound)?;
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    // ── read_dir ──────────────────────────────────────────────────────────────

    fn read_dir<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let fc_path = self.to_fc_path(path);

            // Resolve the directory itself to get its fileid.
            let dir_row = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &fc_path,
            )
            .await
            .ok_or(FsError::NotFound)?;

            // Fetch all direct children.
            let children = row::list_children(
                &self.state.pool,
                &self.state.table_prefix,
                dir_row.fileid,
                self.storage_id,
            )
            .await;

            // Batch-load oc_filecache_extended for every child in a single
            // query instead of N individual queries — so that depth-1 PROPFIND
            // returns correct {nc:}creation_time, {nc:}upload_time, and
            // {nc:}metadata_etag without hitting the DB per-row (REQ §4.1).
            let child_fileids: Vec<i64> = children.iter().map(|c| c.fileid).collect();
            let extended_map = row::list_extended_batch(
                &self.state.pool,
                &self.state.table_prefix,
                &child_fileids,
            )
            .await;

            // Resolve MIME types from cache (no DB round-trip per row).
            let entries: Vec<Result<Box<dyn DavDirEntry>, FsError>> = {
                let cache = self.state.mime_cache.read().expect("mime cache lock");
                children
                    .into_iter()
                    .map(|child| {
                        let mime = cache
                            .get_name(child.mimetype)
                            .unwrap_or("application/octet-stream")
                            .to_string();
                        let mut meta = NcMetaData::from_row(&child, mime, None);
                        // Apply extended times from the batch map.
                        if let Some(ext) = extended_map.get(&child.fileid) {
                            meta.apply_extended(
                                ext.creation_time,
                                ext.upload_time,
                                ext.metadata_etag.clone(),
                            );
                        }
                        Ok(Box::new(NcDirEntry { meta }) as Box<dyn DavDirEntry>)
                    })
                    .collect()
            };

            let s: FsStream<Box<dyn DavDirEntry>> = Box::pin(stream::iter(entries));
            Ok(s)
        }
        .boxed()
    }

    // ── open ──────────────────────────────────────────────────────────────────

    fn open<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let fc_path = self.to_fc_path(path);
            let disk = self.disk_path(&fc_path);

            if options.read && !options.write {
                // ── Read-only ──────────────────────────────────────────────
                let meta = self.load_meta(&fc_path).await.ok_or(FsError::NotFound)?;
                let disk2 = disk.clone();
                let file = blocking(move || std::fs::File::open(&disk2))
                    .await
                    .map_err(io_to_fs)?;
                Ok(Box::new(NcDavFile {
                    file: Some(file),
                    meta,
                    write: None,
                }) as Box<dyn DavFile>)
            } else {
                // ── Write ──────────────────────────────────────────────────
                let existing = row::lookup_by_path(
                    &self.state.pool,
                    &self.state.table_prefix,
                    self.storage_id,
                    &fc_path,
                )
                .await;

                if options.create_new && existing.is_some() {
                    return Err(FsError::Exists);
                }

                // Resolve parent directory.
                let parent_fc_path = {
                    let mut parts: Vec<&str> = fc_path.split('/').collect();
                    parts.pop();
                    if parts.is_empty() {
                        "files".to_string()
                    } else {
                        parts.join("/")
                    }
                };
                let parent_row = row::lookup_by_path(
                    &self.state.pool,
                    &self.state.table_prefix,
                    self.storage_id,
                    &parent_fc_path,
                )
                .await
                .ok_or(FsError::NotFound)?;

                // Resolve MIME type ids.
                let file_name = fc_path.rsplit('/').next().unwrap_or("");
                let ext = file_name.rsplit('.').next().unwrap_or("").to_lowercase();
                let mime_str = mime_guess::from_ext(&ext)
                    .first_raw()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let part_str = mime_str
                    .split('/')
                    .next()
                    .unwrap_or("application")
                    .to_string();

                let (mime_type_id, mimepart_id) = {
                    let cache = self.state.mime_cache.read().expect("mime cache lock");
                    let mid = cache.get_id(&mime_str).unwrap_or(1);
                    let pid = cache.get_id(&format!("{part_str}/")).unwrap_or(1);
                    (mid, pid)
                };

                // Build metadata for the DavFile (may not exist in DB yet).
                let meta = match &existing {
                    Some(row) => {
                        let mime_type = {
                            let cache = self.state.mime_cache.read().expect("mime cache lock");
                            cache
                                .get_name(row.mimetype)
                                .unwrap_or("application/octet-stream")
                                .to_string()
                        };
                        NcMetaData::from_row(row, mime_type, None)
                    }
                    None => NcMetaData {
                        fileid: 0,
                        size: 0,
                        mtime: 0,
                        is_dir_flag: false,
                        mime_type: mime_str.clone(),
                        etag: None,
                        permissions: 27,
                        creation_time: 0,
                        upload_time: 0,
                        checksum: None,
                        display_name: file_name.to_string(),
                        metadata_etag: None,
                        storage: self.storage_id,
                        path: Some(fc_path.clone()),
                        parent: parent_row.fileid,
                    },
                };

                // Create parent dirs on disk if needed.
                let parent_disk = self.disk_path(&parent_fc_path);
                if !parent_disk.exists() {
                    blocking(move || std::fs::create_dir_all(&parent_disk))
                        .await
                        .map_err(io_to_fs)?;
                }

                // Create temp file next to the target.
                let temp_path = {
                    let mut p = disk.clone();
                    let name = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("tmp")
                        .to_string();
                    p.set_file_name(format!(".nc_upload_{name}_{}", uuid::Uuid::new_v4()));
                    p
                };
                let tp2 = temp_path.clone();
                let file = blocking(move || {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(&tp2)
                })
                .await
                .map_err(io_to_fs)?;

                let write_ctx = WriteCtx {
                    temp_path,
                    final_path: disk,
                    pool: self.state.pool.clone(),
                    prefix: self.state.table_prefix.clone(),
                    storage_id: self.storage_id,
                    fc_path,
                    parent_id: parent_row.fileid,
                    uid: self.uid.clone(),
                    mime_type_id,
                    mimepart_id,
                    initial_fileid: existing.map(|r| r.fileid),
                    expected_size: options.size,
                    oc_checksum: options.checksum.clone(),
                    running_hash: crate::davfile::RunningHash::from_checksum_header(
                        options.checksum.as_deref(),
                    ),
                    x_oc_mtime: self.x_oc_mtime,
                    x_oc_ctime: self.x_oc_ctime,
                    write_result: self.write_result.clone(),
                    put_error: self.put_error.clone(),
                };

                Ok(Box::new(NcDavFile {
                    file: Some(file),
                    meta,
                    write: Some(write_ctx),
                }) as Box<dyn DavFile>)
            }
        }
        .boxed()
    }

    // ── create_dir ────────────────────────────────────────────────────────────

    fn create_dir<'a>(&'a self, path: &'a dav_server::davpath::DavPath) -> FsFuture<'a, ()> {
        async move {
            let fc_path = self.to_fc_path(path);
            let disk    = self.disk_path(&fc_path);

            // Must not already exist.
            if row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &fc_path,
            )
            .await
            .is_some()
            {
                return Err(FsError::Exists);
            }

            // Look up parent.
            let parent_path = {
                let mut parts: Vec<&str> = fc_path.split('/').collect();
                parts.pop();
                parts.join("/")
            };
            let parent_row = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &parent_path,
            )
            .await
            .ok_or(FsError::NotFound)?;

            // Create directory on disk.
            blocking(move || std::fs::create_dir(&disk))
                .await
                .map_err(io_to_fs)?;

            // Insert into oc_filecache.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
            let fid  = row::next_fileid(&self.state.pool, &self.state.table_prefix)
                .await
                .map_err(|_| FsError::GeneralFailure)?;

            let dir_mime_id = {
                let cache = self.state.mime_cache.read().expect("mime cache lock");
                cache.get_id("httpd/unix-directory").unwrap_or(2)
            };
            let hash = row::path_hash(&fc_path);
            let name = fc_path.rsplit('/').next().unwrap_or("").to_string();

            let sql = format!(
                "INSERT INTO {prefix}filecache \
                 (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
                  size, mtime, storage_mtime, etag, permissions, checksum, creation_time, upload_time) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)",
                prefix = self.state.table_prefix
            );
            sqlx::query(&sql)
                .bind(fid)
                .bind(self.storage_id)
                .bind(&fc_path)
                .bind(&hash)
                .bind(parent_row.fileid)
                .bind(&name)
                .bind(dir_mime_id)
                .bind(dir_mime_id)
                .bind(0i64)
                .bind(now)
                .bind(now)
                .bind(&etag)
                .bind(31i32)
                .bind("")
                .bind(now)
                .bind(now)
                .execute(&self.state.pool)
                .await
                .map_err(|e| {
                    warn!("create_dir DB insert failed: {e}");
                    FsError::GeneralFailure
                })?;

            Ok(())
        }
        .boxed()
    }

    // ── remove_file ───────────────────────────────────────────────────────────

    fn remove_file<'a>(&'a self, path: &'a dav_server::davpath::DavPath) -> FsFuture<'a, ()> {
        async move {
            let fc_path = self.to_fc_path(path);
            let row = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &fc_path,
            )
            .await
            .ok_or(FsError::NotFound)?;

            let disk = self.disk_path(&fc_path);
            blocking(move || std::fs::remove_file(&disk))
                .await
                .map_err(io_to_fs)?;

            let sql = format!(
                "DELETE FROM {prefix}filecache WHERE fileid = $1",
                prefix = self.state.table_prefix
            );
            sqlx::query(&sql)
                .bind(row.fileid)
                .execute(&self.state.pool)
                .await
                .map_err(|_| FsError::GeneralFailure)?;

            // Clean up extended metadata row (REQ §4.5).
            let sql_ext = format!(
                "DELETE FROM {prefix}filecache_extended WHERE fileid = $1",
                prefix = self.state.table_prefix
            );
            let _ = sqlx::query(&sql_ext)
                .bind(row.fileid)
                .execute(&self.state.pool)
                .await;

            Ok(())
        }
        .boxed()
    }

    // ── remove_dir ────────────────────────────────────────────────────────────

    fn remove_dir<'a>(&'a self, path: &'a dav_server::davpath::DavPath) -> FsFuture<'a, ()> {
        async move {
            let fc_path = self.to_fc_path(path);

            let disk = self.disk_path(&fc_path);
            blocking(move || std::fs::remove_dir_all(&disk))
                .await
                .map_err(io_to_fs)?;

            // Remove the directory and all descendants from oc_filecache.
            let prefix = &self.state.table_prefix;
            let like_pat = format!("{fc_path}/%");

            // Delete extended metadata for all affected rows first (while the
            // filecache rows still exist so the subquery resolves correctly).
            let sql_ext = format!(
                "DELETE FROM {prefix}filecache_extended \
                 WHERE fileid IN (\
                     SELECT fileid FROM {prefix}filecache \
                     WHERE storage = $1 AND (path = $2 OR path LIKE $3)\
                 )"
            );
            let _ = sqlx::query(&sql_ext)
                .bind(self.storage_id)
                .bind(&fc_path)
                .bind(&like_pat)
                .execute(&self.state.pool)
                .await;

            let sql_subtree = format!(
                "DELETE FROM {prefix}filecache WHERE storage = $1 AND (path = $2 OR path LIKE $3)"
            );
            sqlx::query(&sql_subtree)
                .bind(self.storage_id)
                .bind(&fc_path)
                .bind(&like_pat)
                .execute(&self.state.pool)
                .await
                .map_err(|_| FsError::GeneralFailure)?;

            Ok(())
        }
        .boxed()
    }

    // ── rename (MOVE) ─────────────────────────────────────────────────────────

    fn rename<'a>(
        &'a self,
        from: &'a dav_server::davpath::DavPath,
        to: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, ()> {
        async move {
            let from_fc = self.to_fc_path(from);
            let to_fc = self.to_fc_path(to);

            let from_row = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &from_fc,
            )
            .await
            .ok_or(FsError::NotFound)?;

            // Resolve new parent.
            let to_parent_fc = {
                let mut parts: Vec<&str> = to_fc.split('/').collect();
                parts.pop();
                parts.join("/")
            };
            let to_parent = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &to_parent_fc,
            )
            .await
            .ok_or(FsError::NotFound)?;

            // Move on disk.
            let from_disk = self.disk_path(&from_fc);
            let to_disk = self.disk_path(&to_fc);
            blocking(move || std::fs::rename(&from_disk, &to_disk))
                .await
                .map_err(io_to_fs)?;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let new_etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
            let new_name = to_fc.rsplit('/').next().unwrap_or("").to_string();
            let new_hash = row::path_hash(&to_fc);
            let prefix = &self.state.table_prefix;

            // Update the node itself.
            let sql_node = format!(
                "UPDATE {prefix}filecache \
                 SET path=$1, path_hash=$2, name=$3, parent=$4, mtime=$5, etag=$6 \
                 WHERE fileid=$7"
            );
            sqlx::query(&sql_node)
                .bind(&to_fc)
                .bind(&new_hash)
                .bind(&new_name)
                .bind(to_parent.fileid)
                .bind(now)
                .bind(&new_etag)
                .bind(from_row.fileid)
                .execute(&self.state.pool)
                .await
                .map_err(|_| FsError::GeneralFailure)?;

            // Update all descendants (directory move).
            if from_row.mimetype == {
                let cache = self.state.mime_cache.read().expect("mime cache lock");
                cache.get_id("httpd/unix-directory").unwrap_or(0)
            } {
                // Bulk-rename all paths under the old prefix using a Rust-side
                // loop (avoids relying on DB-side MD5 dialect differences).
                let _ = self
                    .rename_subtree_paths(&from_fc, &to_fc, from_row.fileid, prefix)
                    .await;
            }

            Ok(())
        }
        .boxed()
    }

    // ── copy ─────────────────────────────────────────────────────────────────

    fn copy<'a>(
        &'a self,
        from: &'a dav_server::davpath::DavPath,
        to: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, ()> {
        async move {
            let from_fc = self.to_fc_path(from);
            let to_fc   = self.to_fc_path(to);
            let from_disk = self.disk_path(&from_fc);
            let to_disk   = self.disk_path(&to_fc);

            blocking(move || std::fs::copy(&from_disk, &to_disk).map(|_| ()))
                .await
                .map_err(io_to_fs)?;

            // For simplicity: remove old DB row for destination if exists,
            // then insert new row by re-reading disk metadata.
            let _ = sqlx::query(&format!(
                "DELETE FROM {prefix}filecache \
                 WHERE storage = $1 AND path_hash = $2",
                prefix = self.state.table_prefix
            ))
            .bind(self.storage_id)
            .bind(row::path_hash(&to_fc))
            .execute(&self.state.pool)
            .await;

            if let Some(from_row) = row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                &from_fc,
            )
            .await
            {
                let to_parent_fc = {
                    let mut parts: Vec<&str> = to_fc.split('/').collect();
                    parts.pop();
                    parts.join("/")
                };
                if let Some(parent_row) = row::lookup_by_path(
                    &self.state.pool,
                    &self.state.table_prefix,
                    self.storage_id,
                    &to_parent_fc,
                )
                .await
                {
                    let now  = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let fid  = row::next_fileid(&self.state.pool, &self.state.table_prefix)
                        .await
                        .unwrap_or(from_row.fileid + 1);
                    let etag = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
                    let name = to_fc.rsplit('/').next().unwrap_or("").to_string();
                    let hash = row::path_hash(&to_fc);
                    let prefix = &self.state.table_prefix;

                    let sql = format!(
                        "INSERT INTO {prefix}filecache \
                         (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
                          size, mtime, storage_mtime, etag, permissions, checksum, creation_time, upload_time) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)"
                    );
                    let _ = sqlx::query(&sql)
                        .bind(fid)
                        .bind(self.storage_id)
                        .bind(&to_fc)
                        .bind(&hash)
                        .bind(parent_row.fileid)
                        .bind(&name)
                        .bind(from_row.mimetype)
                        .bind(from_row.mimepart)
                        .bind(from_row.size)
                        .bind(now)
                        .bind(now)
                        .bind(&etag)
                        .bind(from_row.permissions)
                        .bind(from_row.checksum.as_deref().unwrap_or(""))
                        .bind(now)
                        .bind(now)
                        .execute(&self.state.pool)
                        .await;
                }
            }
            Ok(())
        }
        .boxed()
    }

    // ── set_modified ──────────────────────────────────────────────────────────

    fn set_modified<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        tm: std::time::SystemTime,
    ) -> FsFuture<'a, ()> {
        async move {
            let fc_path = self.to_fc_path(path);
            let mtime = tm
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let sql = format!(
                "UPDATE {prefix}filecache SET mtime=$1, storage_mtime=$2 WHERE storage=$3 AND path_hash=$4",
                prefix = self.state.table_prefix
            );
            sqlx::query(&sql)
                .bind(mtime)
                .bind(mtime)
                .bind(self.storage_id)
                .bind(row::path_hash(&fc_path))
                .execute(&self.state.pool)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            Ok(())
        }
        .boxed()
    }

    // ── quota ─────────────────────────────────────────────────────────────────

    fn get_quota(&'_ self) -> FsFuture<'_, (u64, Option<u64>)> {
        async move {
            // Query the `files/` root filecache entry.  Nextcloud keeps its
            // `size` column equal to the recursive total for the user's home
            // storage, so this single lookup gives the total used bytes without
            // scanning the full subtree (REQ §6.5, PHASE-4.7).
            let used = match row::lookup_by_path(
                &self.state.pool,
                &self.state.table_prefix,
                self.storage_id,
                "files",
            )
            .await
            {
                Some(r) => r.size.max(0) as u64,
                None => 0,
            };

            // Return `None` for `total` — unlimited quota.
            //
            // dav-server only emits `{DAV:}quota-available-bytes` when the
            // second tuple element is `Some`; by returning `None` we suppress
            // its internal emit.  `get_props()` then injects
            // `quota-available-bytes = -3` (SPACE_UNLIMITED, REQ §6.5) via
            // `build_props()` without producing a duplicate.
            //
            // Per-user quota from `oc_preferences` is deferred to a later phase.
            Ok((used, None))
        }
        .boxed()
    }

    // ── custom DAV properties (oc: / nc: namespaces) ──────────────────────────

    fn have_props<'a>(
        &'a self,
        _path: &'a dav_server::davpath::DavPath,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(future::ready(true))
    }

    fn get_props<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        do_content: bool,
    ) -> FsFuture<'a, Vec<DavProp>> {
        async move {
            let fc_path = self.to_fc_path(path);
            let meta = match self.load_meta(&fc_path).await {
                Some(m) => m,
                None => return Ok(vec![]),
            };

            // Read data-fingerprint from appconfig (REQ §6.5)
            let data_fingerprint = {
                let cache = self.state.appconfig_cache.read().expect("appconfig lock");
                cache
                    .get_string("core", "data-fingerprint")
                    .unwrap_or_default()
            };

            // Count direct children for directories (REQ §6.5 contained-*-count)
            let (child_dirs, child_files) = if meta.is_dir_flag && do_content {
                let dir_mime_id = {
                    let cache = self.state.mime_cache.read().expect("mime cache lock");
                    cache.get_id("httpd/unix-directory").unwrap_or(0)
                };
                row::count_children(
                    &self.state.pool,
                    &self.state.table_prefix,
                    meta.fileid,
                    self.storage_id,
                    dir_mime_id,
                )
                .await
            } else {
                (0, 0)
            };

            // {DAV:}sync-token on collections — RFC 6578 (PHASE-4.11).
            // Query MAX(mtime) of the subtree and format the token value.
            // Only computed for directories when content is requested.
            let sync_token_str: Option<String> = if meta.is_dir_flag && do_content {
                let fc_path = meta.path.as_deref().unwrap_or("files");
                let max_mtime = row::get_subtree_max_mtime(
                    &self.state.pool,
                    &self.state.table_prefix,
                    self.storage_id,
                    fc_path,
                )
                .await;
                Some(format!("http://sabre.io/ns/sync/{max_mtime}"))
            } else {
                None
            };

            // Resolve {oc:}owner-display-name from oc_users.displayname (REQ §6.5 / §4.8).
            // Falls back to the raw UID when no display name is set.
            let owner_display_name = row::lookup_user_display_name(
                &self.state.pool,
                &self.state.table_prefix,
                &self.uid,
            )
            .await;

            // ── Phase 7.6: is_mounted, share_permissions, download_url, note ──
            //
            // `is_mounted`: true when the file lives on a non-home storage.
            // Optimisation: if meta.storage == self.storage_id the FS was
            // already constructed from a home:: storage lookup, so skip DB.
            let is_mounted = if meta.storage == self.storage_id {
                false
            } else {
                row::get_storage_string_id(
                    &self.state.pool,
                    &self.state.table_prefix,
                    meta.storage,
                )
                .await
                .map(|id| !id.starts_with("home::"))
                .unwrap_or(false)
            };

            // `share_permissions`: MAX(permissions) from oc_share for this
            // file and the authenticated user.  Default 31 (all) when no row.
            let share_permissions = row::get_share_max_permissions(
                &self.state.pool,
                &self.state.table_prefix,
                &self.uid,
                meta.fileid,
            )
            .await;

            // `note`: most-recent non-empty share note for this file.
            let note = row::get_share_note(
                &self.state.pool,
                &self.state.table_prefix,
                meta.fileid,
            )
            .await;

            // `download_url`: direct WebDAV URL for home-storage files.
            // Format: {overwrite.cli.url}/remote.php/webdav/{path-without-files-prefix}
            // Empty for non-home storage (object/S3 URLs require storage-specific
            // signed-URL support which is out of scope, PHASE-7.6).
            // Only generated for files (not directories) and when base_url is set.
            let download_url = if !is_mounted
                && !meta.is_dir_flag
                && !self.state.base_url.is_empty()
            {
                // `meta.path` is like "files/Photos/img.jpg"; strip "files" prefix
                // to get the WebDAV subpath "/Photos/img.jpg".
                let subpath = meta.path.as_deref().unwrap_or("").trim_start_matches("files");
                let base = self.state.base_url.trim_end_matches('/');
                format!("{base}/remote.php/webdav{}", percent_encode_path(subpath))
            } else {
                String::new()
            };

            let instance_id = &self.state.instance_id;
            Ok(crate::props::build_props(
                &meta,
                instance_id,
                &self.uid,
                &owner_display_name,
                do_content,
                &data_fingerprint,
                child_dirs,
                child_files,
                sync_token_str.as_deref(),
                is_mounted,
                share_permissions,
                &download_url,
                &note,
            ))
        }
        .boxed()
    }

    fn patch_props<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        patch: Vec<(bool, DavProp)>,
    ) -> FsFuture<'a, Vec<(http::StatusCode, DavProp)>> {
        async move {
            let fc_path = self.to_fc_path(path);
            let hash    = row::path_hash(&fc_path);
            let mut results = Vec::new();

            for (set, prop) in patch {
                let ns   = prop.namespace.as_deref().unwrap_or("");
                let name = prop.name.as_str();

                let status = if set {
                    match (ns, name) {
                        // ── Standard DAV writable props (REQ §6.6) ───────────

                        // {DAV:}getetag — set custom ETag
                        ("DAV:", "getetag") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                let etag = val.trim().trim_matches('"').to_string();
                                let sql = format!(
                                    "UPDATE {prefix}filecache SET etag=$1 \
                                     WHERE storage=$2 AND path_hash=$3",
                                    prefix = self.state.table_prefix
                                );
                                let ok = sqlx::query(&sql)
                                    .bind(&etag)
                                    .bind(self.storage_id)
                                    .bind(&hash)
                                    .execute(&self.state.pool)
                                    .await
                                    .is_ok();
                                if ok { http::StatusCode::OK } else { http::StatusCode::INTERNAL_SERVER_ERROR }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        // {DAV:}getlastmodified / {DAV:}lastmodified — update mtime
                        ("DAV:", "getlastmodified") | ("DAV:", "lastmodified") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                // RFC 1123 date OR Unix timestamp integer
                                let ts_opt = val.trim().parse::<i64>().ok().or_else(|| {
                                    httpdate::parse_http_date(val.trim())
                                        .ok()
                                        .and_then(|st| {
                                            st.duration_since(std::time::UNIX_EPOCH)
                                                .ok()
                                                .map(|d| d.as_secs() as i64)
                                        })
                                });
                                if let Some(ts) = ts_opt {
                                    let sql = format!(
                                        "UPDATE {prefix}filecache SET mtime=$1, storage_mtime=$2 \
                                         WHERE storage=$3 AND path_hash=$4",
                                        prefix = self.state.table_prefix
                                    );
                                    let _ = sqlx::query(&sql)
                                        .bind(ts).bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(&self.state.pool)
                                        .await;
                                    http::StatusCode::OK
                                } else {
                                    http::StatusCode::BAD_REQUEST
                                }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        // {DAV:}creationdate — set creation time (ISO 8601)
                        //
                        // When the oc_filecache_extended row does not yet exist we INSERT it,
                        // reading upload_time from oc_filecache so that the sibling column is
                        // preserved rather than zeroed (REQ §6.6, PHASE-4.10).
                        ("DAV:", "creationdate") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                if let Some(ts) = parse_iso8601(val.trim()) {
                                    let sql = format!(
                                        "INSERT INTO {prefix}filecache_extended \
                                         (fileid, creation_time, metadata_etag, upload_time) \
                                         SELECT fileid, $1, NULL, upload_time FROM {prefix}filecache \
                                         WHERE storage=$2 AND path_hash=$3 \
                                         ON CONFLICT(fileid) DO UPDATE SET creation_time=excluded.creation_time",
                                        prefix = self.state.table_prefix
                                    );
                                    let _ = sqlx::query(&sql)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(&self.state.pool)
                                        .await;
                                    http::StatusCode::OK
                                } else {
                                    http::StatusCode::BAD_REQUEST
                                }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        // {DAV:}displayname — explicitly blocked (REQ §6.6)
                        ("DAV:", "displayname") => http::StatusCode::FORBIDDEN,

                        // ── NC writable props ─────────────────────────────────
                        //
                        // For each nc: time property, when the oc_filecache_extended row is
                        // absent we INSERT it, reading the *other* timestamp column from
                        // oc_filecache so neither value is zeroed (REQ §6.6, PHASE-4.10).
                        ("http://nextcloud.org/ns", "creation_time") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                if let Ok(ts) = val.trim().parse::<i64>() {
                                    let sql_upsert = format!(
                                        "INSERT INTO {prefix}filecache_extended \
                                         (fileid, creation_time, metadata_etag, upload_time) \
                                         SELECT fileid, $1, NULL, upload_time FROM {prefix}filecache \
                                         WHERE storage = $2 AND path_hash = $3 \
                                         ON CONFLICT(fileid) DO UPDATE SET creation_time = excluded.creation_time",
                                        prefix = self.state.table_prefix,
                                    );
                                    let _ = sqlx::query(&sql_upsert)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(&self.state.pool)
                                        .await;
                                    http::StatusCode::OK
                                } else {
                                    http::StatusCode::BAD_REQUEST
                                }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        // {nc:}upload_time — update upload time (unix int).
                        // Preserve creation_time from oc_filecache when inserting a new
                        // extended row (PHASE-4.10).
                        ("http://nextcloud.org/ns", "upload_time") => {
                            if let Some(val) = extract_text_from_prop_xml(&prop) {
                                if let Ok(ts) = val.trim().parse::<i64>() {
                                    let sql_upsert = format!(
                                        "INSERT INTO {prefix}filecache_extended \
                                         (fileid, upload_time, metadata_etag, creation_time) \
                                         SELECT fileid, $1, NULL, creation_time FROM {prefix}filecache \
                                         WHERE storage = $2 AND path_hash = $3 \
                                         ON CONFLICT(fileid) DO UPDATE SET upload_time = excluded.upload_time",
                                        prefix = self.state.table_prefix,
                                    );
                                    let _ = sqlx::query(&sql_upsert)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(&self.state.pool)
                                        .await;
                                    http::StatusCode::OK
                                } else {
                                    http::StatusCode::BAD_REQUEST
                                }
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        }

                        _ => http::StatusCode::FORBIDDEN,
                    }
                } else {
                    // DELETE — NC built-in props are not deletable
                    http::StatusCode::FORBIDDEN
                };
                results.push((status, prop));
            }
            Ok(results)
        }
        .boxed()
    }
}

// ─── Helper methods ────────────────────────────────────────────────────────────

impl NcFileSystem {
    /// Rename all oc_filecache paths below `old_prefix` to `new_prefix`.
    ///
    /// This fetches every matching row and re-inserts the path_hash in Rust
    /// to avoid relying on a DB-side MD5 function that may not exist in SQLite.
    async fn rename_subtree_paths(
        &self,
        old_prefix: &str,
        new_prefix: &str,
        _dir_fileid: i64,
        prefix: &str,
    ) {
        let like = format!("{old_prefix}/%");
        let sql_fetch =
            format!("SELECT fileid, path FROM {prefix}filecache WHERE storage = $1 AND path LIKE $2");
        let rows = sqlx::query(&sql_fetch)
            .bind(self.storage_id)
            .bind(&like)
            .fetch_all(&self.state.pool)
            .await
            .unwrap_or_default();

        for r in rows {
            let old_path: String = r.get("path");
            let new_path = format!("{new_prefix}{}", &old_path[old_prefix.len()..]);
            let new_hash = row::path_hash(&new_path);
            let fileid: i64 = r.get("fileid");
            let sql_upd =
                format!("UPDATE {prefix}filecache SET path=$1, path_hash=$2 WHERE fileid=$3");
            let _ = sqlx::query(&sql_upd)
                .bind(&new_path)
                .bind(&new_hash)
                .bind(fileid)
                .execute(&self.state.pool)
                .await;
        }
    }
}

// ─── XML prop value extractor ─────────────────────────────────────────────────

/// Percent-encode a URI path string, preserving `/` as a segment separator.
///
/// Encodes any byte that is not an RFC 3986 unreserved character (`A-Za-z0-9-._~`)
/// or one of the path-allowed characters (`/:@!$&'()*+,;=`).
/// Used to build `{oc:}downloadURL` values (PHASE-7.6).
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'.' | b'_' | b'~' | b'/'
            | b':' | b'@' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')'
            | b'*' | b'+' | b',' | b';' | b'=' => out.push(byte as char),
            _ => {
                out.push('%');
                let hi = byte >> 4;
                let lo = byte & 0xF;
                out.push(char::from_digit(hi as u32, 16).unwrap().to_ascii_uppercase());
                out.push(char::from_digit(lo as u32, 16).unwrap().to_ascii_uppercase());
            }
        }
    }
    out
}

/// Extract the text content from a `DavProp`'s raw XML bytes.
/// e.g. `<nc:creation_time xmlns:nc="...">1700000000</nc:creation_time>` → `"1700000000"`
fn extract_text_from_prop_xml(prop: &DavProp) -> Option<String> {
    let xml = prop.xml.as_ref()?;
    let s = std::str::from_utf8(xml).ok()?;
    let start = s.find('>')? + 1;
    let end = s.rfind('<')?;
    if start < end {
        Some(s[start..end].to_string())
    } else {
        None
    }
}

// ─── ISO 8601 parser ──────────────────────────────────────────────────────────

/// Parse an ISO 8601 / RFC 3339 datetime string to a Unix timestamp (UTC).
///
/// Handles the subset used by WebDAV `{DAV:}creationdate`: `YYYY-MM-DDTHH:MM:SSZ`
/// and `YYYY-MM-DDTHH:MM:SS+HH:MM` / `YYYY-MM-DDTHH:MM:SS-HH:MM`.
/// Timezone offsets are **ignored** (all times treated as UTC) for simplicity.
fn parse_iso8601(s: &str) -> Option<i64> {
    // Strip timezone suffix to get at most "YYYY-MM-DDTHH:MM:SS"
    let core = if s.ends_with('Z') {
        &s[..s.len() - 1]
    } else if let Some(pos) = s.rfind('+').filter(|&p| p >= 10) {
        &s[..pos]
    } else if let Some(pos) = s[10..].rfind('-').map(|p| p + 10) {
        &s[..pos]
    } else {
        s
    };

    if core.len() < 19 {
        return None;
    }

    let year: i64 = core[0..4].parse().ok()?;
    let month: i64 = core[5..7].parse().ok()?;
    let day: i64 = core[8..10].parse().ok()?;
    let hour: i64 = core[11..13].parse().ok()?;
    let min: i64 = core[14..16].parse().ok()?;
    let sec: i64 = core[17..19].parse().ok()?;

    // Julian Day Number → Unix epoch (days)
    let a = (14 - month) / 12;
    let y = year + 4800 - a;
    let m = month + 12 * a - 3;
    let jdn = day + (153 * m + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32045;
    let unix_days = jdn - 2_440_588; // JDN of 1970-01-01

    Some(unix_days * 86_400 + hour * 3_600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::parse_iso8601;

    #[test]
    fn iso8601_z_suffix() {
        // python3: datetime(2024,1,15,12,34,56,tz=UTC).timestamp() == 1705322096
        assert_eq!(parse_iso8601("2024-01-15T12:34:56Z"), Some(1705322096));
    }

    #[test]
    fn iso8601_plus_offset_ignored() {
        // Offset is stripped so both forms give the same raw numbers treated as UTC
        assert_eq!(parse_iso8601("2024-01-15T12:34:56+02:00"), Some(1705322096));
    }

    #[test]
    fn iso8601_unix_epoch() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
    }
}
