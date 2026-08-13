//! `NcMetaData` and `NcDirEntry` — the `DavMetaData` / `DavDirEntry` impls
//! backed by rows from `oc_filecache`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dav_server::fs::{DavDirEntry, DavMetaData, FsFuture, FsResult};
use futures::future;

use crate::row::FileCacheRow;

// ─── NcMetaData ───────────────────────────────────────────────────────────────

/// Metadata for a single `oc_filecache` node, enriched with the MIME type
/// string and the `oc_filecache_extended` fields.
#[derive(Debug, Clone)]
pub struct NcMetaData {
    pub fileid: i64,
    pub size: u64,
    /// Unix timestamp (seconds since epoch) of the last modification.
    pub mtime: i64,
    pub is_dir_flag: bool,
    /// Arc-shared mime string (task 23.4) — one allocation per distinct mime
    /// across a listing's children instead of one to_string per child.
    pub mime_type: Arc<str>,
    pub etag: Option<String>,
    pub permissions: i32,
    /// Creation time (from `oc_filecache_extended` when available).
    pub creation_time: i64,
    /// Upload time (from `oc_filecache_extended` when available).
    pub upload_time: i64,
    pub checksum: Option<String>,
    /// File name (leaf component of the path).
    pub display_name: String,
    /// `{nc:}metadata_etag` from `oc_filecache_extended`.
    pub metadata_etag: Option<String>,
    /// Foreign key into `oc_storages`.
    pub storage: i64,
    /// Full path within the storage (e.g. `files/Photos/img.jpg`).
    pub path: Option<String>,
    /// Parent `fileid`.
    pub parent: i64,
}

impl NcMetaData {
    /// Build `NcMetaData` from a filecache row, resolved MIME type string, and
    /// optional extended metadata.
    pub fn from_row(row: &FileCacheRow, mime_type: Arc<str>, metadata_etag: Option<String>) -> Self {
        let is_dir = mime_type.as_ref() == "httpd/unix-directory";
        NcMetaData {
            fileid: row.fileid,
            size: row.size.max(0) as u64,
            mtime: row.mtime,
            is_dir_flag: is_dir,
            mime_type,
            etag: row.etag.clone(),
            permissions: row.permissions,
            creation_time: row.creation_time,
            upload_time: row.upload_time,
            checksum: row.checksum.clone(),
            display_name: row.name.clone().unwrap_or_default(),
            metadata_etag,
            storage: row.storage,
            path: row.path.clone(),
            parent: row.parent,
        }
    }

    /// Return the MIME type stored in `oc_filecache` / `oc_mimetypes` for this
    /// node.
    ///
    /// This is the authoritative content type for `{DAV:}getcontenttype` —
    /// recorded at upload time from the file extension.  The dav-server
    /// internally also derives content type from the URL extension
    /// (`path.get_mime_type_str()`); in practice the two values agree for any
    /// normally-uploaded file.  This method exposes the stored value so it can
    /// be used wherever the DB-backed type is explicitly required (e.g. the
    /// `Content-Type` header on GET responses).
    pub fn content_type(&self) -> &str {
        &self.mime_type
    }

    /// Prefer the authoritative extended times where non-zero.
    pub fn apply_extended(
        &mut self,
        creation_time: i64,
        upload_time: i64,
        metadata_etag: Option<String>,
    ) {
        if creation_time > 0 {
            self.creation_time = creation_time;
        }
        if upload_time > 0 {
            self.upload_time = upload_time;
        }
        if metadata_etag.is_some() {
            self.metadata_etag = metadata_etag;
        }
    }
}

impl DavMetaData for NcMetaData {
    fn len(&self) -> u64 {
        self.size
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(UNIX_EPOCH + Duration::from_secs(self.mtime.max(0) as u64))
    }

    fn is_dir(&self) -> bool {
        self.is_dir_flag
    }

    fn created(&self) -> FsResult<SystemTime> {
        Ok(UNIX_EPOCH + Duration::from_secs(self.creation_time.max(0) as u64))
    }

    /// Return the ETag stored in `oc_filecache.etag`, quoted per RFC 4918 §8.8.
    /// PHP's `SabreDAV` wraps the raw DB value in double quotes for both
    /// `{DAV:}getetag` in the XML body and the `ETag` response header.
    fn etag(&self) -> Option<String> {
        self.etag.as_ref().map(|e| format!("\"{e}\""))
    }
}

// ─── NcDirEntry ───────────────────────────────────────────────────────────────

/// One entry returned by `read_dir()`.  Metadata is pre-loaded from the DB
/// so `DavDirEntry::metadata()` returns immediately without a second query.
#[derive(Debug, Clone)]
pub struct NcDirEntry {
    /// Arc-shared with the batch map (task 23.4) — the deep clone happens
    /// once in read_dir; metadata() clones on demand.
    pub meta: Arc<NcMetaData>,
}

impl DavDirEntry for NcDirEntry {
    fn name(&self) -> Vec<u8> {
        self.meta.display_name.as_bytes().to_vec()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta: Box<dyn DavMetaData> = Box::new((*self.meta).clone());
        Box::pin(future::ok(meta))
    }

    fn is_dir(&'_ self) -> FsFuture<'_, bool> {
        Box::pin(future::ok(self.meta.is_dir_flag))
    }

    fn is_file(&'_ self) -> FsFuture<'_, bool> {
        Box::pin(future::ok(!self.meta.is_dir_flag))
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row::FileCacheRow;

    fn dummy_row(_mime_type: &str) -> FileCacheRow {
        FileCacheRow {
            fileid: 1,
            storage: 1,
            path: Some("files/test.txt".into()),
            path_hash: "abc".into(),
            parent: 0,
            name: Some("test.txt".into()),
            mimetype: 1,
            mimepart: 1,
            size: 42,
            mtime: 1700000000,
            storage_mtime: 1700000000,
            etag: Some("etag42".into()),
            permissions: 27,
            checksum: None,
            creation_time: 0,
            upload_time: 0,
        }
    }

    #[test]
    fn content_type_returns_stored_mime() {
        let meta = NcMetaData::from_row(&dummy_row("text/plain"), "text/plain".into(), None);
        assert_eq!(meta.content_type(), "text/plain");
    }

    #[test]
    fn content_type_directory_is_unix_dir() {
        let meta = NcMetaData::from_row(
            &dummy_row("httpd/unix-directory"),
            "httpd/unix-directory".into(),
            None,
        );
        assert_eq!(meta.content_type(), "httpd/unix-directory");
        assert!(meta.is_dir());
    }

    #[test]
    fn content_type_survives_apply_extended() {
        let mut meta = NcMetaData::from_row(&dummy_row("image/jpeg"), "image/jpeg".into(), None);
        meta.apply_extended(1710000000, 1710000001, None);
        // MIME must not be mutated by apply_extended
        assert_eq!(meta.content_type(), "image/jpeg");
    }
}
