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
use std::sync::Arc;

use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, DavProp, FsError, FsFuture, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures::{future, FutureExt};
use tokio::task;

use crate::{
    davfile::{NcDavFile, WriteCtx},
    fadvise::Advice,
    metadata::NcMetaData,
    propagator::Propagator,
    propfind::{batch_get, PropfindBatch},
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
    /// Cache propagator — created once per request, shared by all mutations
    /// within the request.  Cheap to clone.
    pub(crate) propagator: Propagator,
    /// `X-NC-Skip-Trashbin: true` header from the request.  When set, DELETE
    /// operations hard-delete instead of moving to trash (§9.3 critique #7).
    pub(crate) skip_trashbin: bool,
    /// Per-request tag cache (§9.5).  Prefetched during `read_dir` for depth-1
    /// PROPFIND, then read by `get_props` for each node.
    pub(crate) tag_cache: crate::tags::TagCache,
    /// Per-request batched PROPFIND data (Phase 18.1).  `read_dir` fetches
    /// everything `get_props` needs for every child in a handful of batched
    /// queries and stores it here; `get_props` reads the cache instead of
    /// re-querying per node.  Nodes outside the batch (the depth-0 root,
    /// which dav-server-rs visits before `read_dir`) fall back to the
    /// single-row queries.
    pub(crate) propfind_batch: PropfindBatch,
    /// NEXTCLOUD-RS PATCH (PHASE-22 T6.5): the client's explicitly requested
    /// property set `(namespace, name)` for `<prop>` PROPFIND requests, set
    /// by dav-server's `handle_propfind` before any read_dir work.  `None`
    /// for allprop/propname (everything requested).  Gates the read_dir
    /// batch families (T6.6).  A plain field is fine: dav-server clones the
    /// fs into `PropWriter` only after the setter runs, and `read_dir` runs
    /// on the instance the setter was called on.
    pub(crate) requested_props: Option<Vec<(Option<String>, String)>>,
    /// The propfind's target path (fc-normalized) + Depth header — set by
    /// `set_propfind_request`; the rich-workspace depth-skip (2026-08-14).
    pub(crate) propfind_target: String,
    pub(crate) propfind_depth: u8,
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
        skip_trashbin: bool,
    ) -> Self {
        let propagator =
            Propagator::new(state.pool.clone(), state.table_prefix.clone(), storage_id);
        let tag_cache = crate::tags::new_tag_cache();
        NcFileSystem {
            state,
            uid,
            storage_id,
            x_oc_mtime,
            x_oc_ctime,
            write_result,
            put_error,
            propagator,
            skip_trashbin,
            tag_cache,
            propfind_batch: PropfindBatch::default(),
            requested_props: None,
            propfind_target: String::new(),
            propfind_depth: 0,
        }
    }

    /// Is `(ns, name)` requested by the current PROPFIND request?
    ///
    /// `None` (allprop / propname / non-PROPFIND requests) means every
    /// property is requested.  `Some(list)` is the client's explicit
    /// `<d:prop>` set (PHASE-22 T6.5).
    pub(crate) fn prop_requested(&self, ns: &str, name: &str) -> bool {
        match &self.requested_props {
            None => true,
            Some(list) => list
                .iter()
                .any(|(n, nm)| n.as_deref() == Some(ns) && nm == name),
        }
    }

    /// Any requested prop in `ns` whose name starts with `prefix` (the
    /// `nc:metadata-*` family is open-ended — the keys come from the
    /// per-file metadata row, not a fixed list).
    pub(crate) fn prop_requested_prefix(&self, ns: &str, prefix: &str) -> bool {
        match &self.requested_props {
            None => true,
            Some(list) => list
                .iter()
                .any(|(n, nm)| n.as_deref() == Some(ns) && nm.starts_with(prefix)),
        }
    }

    /// Convert a `DavPath` to an `oc_filecache` path.
    pub(crate) fn to_fc_path(&self, path: &dav_server::davpath::DavPath) -> String {
        let raw = String::from_utf8_lossy(path.as_bytes()).into_owned();
        dav_to_fc_path(&raw)
    }

    /// Resolve the on-disk path for a filecache path.
    pub(crate) fn disk_path(&self, fc_path: &str) -> PathBuf {
        let data_dir = self.state.data_directory.as_path();
        disk_path(data_dir, &self.uid, fc_path)
    }

    /// Ensure a parent directory exists in the filecache, creating it
    /// recursively if needed.
    ///
    /// Matches PHP's `View::createParentDirectories()` which is called
    /// before every `newFile()` / `newFolder()` operation.  Without this,
    /// uploading a file into a newly-created folder (or a folder that only

    /// Load `NcMetaData` for any filecache path, including extended times.
    pub(crate) async fn load_meta(&self, fc_path: &str) -> Option<NcMetaData> {
        // Phase 18.1: serve from the per-request batch populated by `read_dir`
        // — depth-1 PROPFIND's per-child lookups reuse the rows already in
        // hand instead of re-issuing two queries per node.  Round-3 Task 10:
        // additionally store on miss, so the root's double lookup per
        // PROPFIND (`fs.metadata(root)` then `get_props(root)` →
        // `load_meta(root)`) pays once.  All callers are read-only —
        // `metadata()`, the `open()` read path, and `get_props`; the write
        // paths call `lookup_by_path` directly and never consult the batch,
        // so an entry cannot go stale within a request.
        let key = fc_path.trim_end_matches('/');
        if let Some(meta) = {
            let inner = self
                .propfind_batch
                .inner
                .lock()
                .expect("propfind batch lock");
            batch_get(&inner, |i| &i.meta, key)
        } {
            return Some((*meta).clone());
        }
        let found = row::lookup_by_path_with_ext(
            &self.state.pool,
            &self.state.table_prefix,
            self.storage_id,
            fc_path,
        )
        .await;
        tracing::trace!(
            fc_path = %fc_path,
            storage_id = self.storage_id,
            found = found.is_some(),
            "load_meta result"
        );
        let (row, ext) = found?;

        let mime_type = {
            let cache = self.state.mime_cache.read().expect("mime cache lock");
            cache
                .get_name(row.mimetype)
                .unwrap_or_else(|| Arc::from("application/octet-stream"))
        };

        let mut meta = NcMetaData::from_row(&row, mime_type, ext.metadata_etag.clone());
        meta.apply_extended(ext.creation_time, ext.upload_time, ext.metadata_etag);
        let arc = Arc::new(meta);
        self.propfind_batch
            .inner
            .lock()
            .expect("propfind batch lock")
            .meta
            .insert(key.to_string(), arc.clone());
        Some((*arc).clone())
    }
}

// ─── blocking helper ──────────────────────────────────────────────────────────

pub(crate) async fn blocking<F, R>(func: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    // Always the blocking pool (task 23.3) — see davfile.rs.
    task::spawn_blocking(func).await.unwrap()
}

pub(crate) fn io_to_fs(e: io::Error) -> FsError {
    match e.kind() {
        io::ErrorKind::NotFound => FsError::NotFound,
        io::ErrorKind::PermissionDenied => FsError::Forbidden,
        io::ErrorKind::AlreadyExists => FsError::Exists,
        _ => FsError::GeneralFailure,
    }
}

// ─── DavFileSystem impl ────────────────────────────────────────────────────────

impl DavFileSystem for NcFileSystem {
    // ── requested props (PHASE-22 T6.5) ─────────────────────────────────────

    fn set_propfind_request(
        &mut self,
        requested: Option<Vec<(Option<String>, String)>>,
        path: &dav_server::davpath::DavPath,
        depth: u8,
    ) {
        self.requested_props = requested;
        // The propfind target (fc-path normalized) + Depth — the
        // rich-workspace depth-skip needs the target-vs-child distinction
        // (text app WorkspacePlugin, 2026-08-14).
        let raw = String::from_utf8_lossy(path.as_bytes()).into_owned();
        self.propfind_target = dav_to_fc_path(&raw);
        self.propfind_depth = depth;
    }

    // ── metadata ─────────────────────────────────────────────────────────────

    fn metadata<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let fc_path = self.to_fc_path(path);
            tracing::trace!(
                dav_path = %String::from_utf8_lossy(path.as_bytes()),
                fc_path = %fc_path,
                storage_id = self.storage_id,
                "metadata lookup"
            );
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
        self.read_dir_batched(path).boxed()
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
                // Task 23.6: the 10 ms HDD seek overlaps the ~0.05 ms DB
                // query — open + WILLNEED kick the kernel readahead now, and
                // `load_meta` runs while the platter moves.  SEQUENTIAL
                // doubles the kernel readahead for the page-by-page GET
                // stream.  (A DB-missing file still 404s — the open is
                // dropped harmlessly.)
                let disk2 = disk.clone();
                let file = blocking(move || std::fs::File::open(&disk2))
                    .await
                    .map_err(io_to_fs)?;
                crate::fadvise::hint(&file, Advice::WillNeed);
                crate::fadvise::hint(&file, Advice::Sequential);
                let meta = self.load_meta(&fc_path).await.ok_or(FsError::NotFound)?;
                Ok(Box::new(NcDavFile {
                    file: Some(file),
                    meta,
                    write: None,
                    file_io: self.state.file_io_permits.clone(),
                    read_buf: bytes::BytesMut::new(),
                    streamed: 0,
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

                // Resolve parent directory (auto-create if missing — matches PHP's
                // $userFolder->newFile() which calls createParentDirectories()).
                let parent_fc_path = {
                    let mut parts: Vec<&str> = fc_path.split('/').collect();
                    parts.pop();
                    if parts.is_empty() {
                        "files".to_string()
                    } else {
                        parts.join("/")
                    }
                };
                let parent_row = self
                    .ensure_parent_dir(&parent_fc_path)
                    .await
                    .map_err(|_| FsError::NotFound)?;

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

                // §10.8: get-or-insert; mimepart without trailing slash
                let (mime_type_id, mimepart_id) = {
                    let mid = nc_db::mime::get_or_insert_mime_id(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.state.mime_cache,
                        &mime_str,
                    )
                    .await;
                    let pid = nc_db::mime::get_or_insert_mime_id(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.state.mime_cache,
                        &part_str,
                    )
                    .await;
                    (mid, pid)
                };

                // Build metadata for the DavFile (may not exist in DB yet).
                let meta = match &existing {
                    Some(row) => {
                        let mime_type = {
                            let cache = self.state.mime_cache.read().expect("mime cache lock");
                            cache
                                .get_name(row.mimetype)
                                .unwrap_or_else(|| Arc::from("application/octet-stream"))
                        };
                        NcMetaData::from_row(row, mime_type, None)
                    }
                    None => NcMetaData {
                        fileid: 0,
                        size: 0,
                        mtime: 0,
                        is_dir_flag: false,
                        mime_type: Arc::from(mime_str.clone()),
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

                let old_size = existing.as_ref().map(|r| r.size).unwrap_or(0);
                let old_mtime = existing.as_ref().map(|r| r.mtime).unwrap_or(0);
                let old_mimetype = existing.as_ref().map(|r| r.mimetype).unwrap_or(0);
                let old_etag = existing.as_ref().and_then(|r| r.etag.clone());
                let old_storage_mtime = existing.as_ref().map(|r| r.storage_mtime).unwrap_or(0);
                // §9.4: inherit permissions + extended times from the source file
                // so that the version row carries correct metadata for PHP-FPM.
                let old_permissions = existing.as_ref().map(|r| r.permissions).unwrap_or(27);
                let (old_creation_time, old_upload_time) = if let Some(ref ex) = existing {
                    let ext =
                        row::get_extended(&self.state.pool, &self.state.table_prefix, ex.fileid)
                            .await;
                    (ext.creation_time, ext.upload_time)
                } else {
                    (0, 0)
                };
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
                    old_size,
                    old_mtime,
                    old_mimetype,
                    old_permissions,
                    old_creation_time,
                    old_upload_time,
                    old_etag,
                    dir_mime_id: self.state.dir_mime_id,
                    dir_mimepart_id: self.state.dir_mimepart_id,
                    old_storage_mtime,
                    expected_size: options.size,
                    oc_checksum: options.checksum.clone(),
                    running_hash: crate::davfile::RunningHash::from_checksum_header(
                        options.checksum.as_deref(),
                    ),
                    x_oc_mtime: self.x_oc_mtime,
                    x_oc_ctime: self.x_oc_ctime,
                    // §10.5 + improvements.md: media-mtime fallback inputs.
                    // is_media from the detected mimetype part ("image"/"video");
                    // arrival_anchor captured at open() — the server-observed
                    // request arrival (the fallback window anchors on it).
                    is_media: part_str == "image" || part_str == "video",
                    arrival_anchor: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64,
                    media_mtime_ctime_fallback: self.state.media_mtime_ctime_fallback,
                    write_result: self.write_result.clone(),
                    put_error: self.put_error.clone(),
                    propagator: self.propagator.clone(),
                    data_dir: self.state.data_directory.clone(),
                    mime_cache: self.state.mime_cache.clone(),
                    instance_id: self.state.instance_id.clone(),
                };

                Ok(Box::new(NcDavFile {
                    file: Some(file),
                    meta,
                    write: Some(write_ctx),
                    file_io: self.state.file_io_permits.clone(),
                    read_buf: bytes::BytesMut::new(),
                    streamed: 0,
                }) as Box<dyn DavFile>)
            }
        }
        .boxed()
    }

    // ── create_dir ──────────────────

    fn create_dir<'a>(&'a self, path: &'a dav_server::davpath::DavPath) -> FsFuture<'a, ()> {
        self.create_dir_row(path).boxed()
    }

    // ── remove_file ───────────────────────────────────────────────────────────

    fn remove_file<'a>(&'a self, path: &'a dav_server::davpath::DavPath) -> FsFuture<'a, ()> {
        async move {
            let fc_path = self.to_fc_path(path);
            self.delete_file(&fc_path).await
        }
        .boxed()
    }

    // ── remove_dir ────────────────────────────────────────────────────────────

    fn remove_dir<'a>(&'a self, path: &'a dav_server::davpath::DavPath) -> FsFuture<'a, ()> {
        async move {
            let fc_path = self.to_fc_path(path);
            self.delete_dir(&fc_path).await
        }
        .boxed()
    }

    // ── rename (MOVE) ───────────────

    fn rename<'a>(
        &'a self,
        from: &'a dav_server::davpath::DavPath,
        to: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, ()> {
        self.rename_node(from, to).boxed()
    }

    // ── copy ──────────────────────

    fn copy<'a>(
        &'a self,
        from: &'a dav_server::davpath::DavPath,
        to: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, ()> {
        self.copy_node(from, to).boxed()
    }

    // ── set_modified ──────────────────────────────────────────────────────────

    // ── set_modified ────────────────

    fn set_modified<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        tm: std::time::SystemTime,
    ) -> FsFuture<'a, ()> {
        self.set_mtime(path, tm).boxed()
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

            // Unlimited quota: return `None` for total — dav-server emits
            // the Nextcloud SPACE_UNLIMITED sentinel `-3` (REQ §6.5) for
            // DIRECTORY nodes when the total is absent (2026-08-14 vendored
            // patch); files answer a 404 propstat like PHP's FilesPlugin.
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
        self.collect_props(path, do_content).boxed()
    }

    fn patch_props<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        patch: Vec<(bool, DavProp)>,
    ) -> FsFuture<'a, Vec<(http::StatusCode, DavProp)>> {
        self.patch_props_inner(path, patch).boxed()
    }
}
