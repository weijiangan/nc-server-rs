// no extra imports needed


// ─── Types ────────────────────────────────────────────────────────────────────

/// One row from `oc_filecache`.
#[derive(Debug, Clone)]
pub struct FileCacheRow {
    pub fileid: i64,
    pub storage: i64,
    pub path: Option<String>,
    pub path_hash: String,
    pub parent: i64,
    pub name: Option<String>,
    pub mimetype: i64,
    pub mimepart: i64,
    pub size: i64,
    pub mtime: i64,
    pub storage_mtime: i64,
    pub etag: Option<String>,
    pub permissions: i32,
    pub checksum: Option<String>,
    pub creation_time: i64,
    pub upload_time: i64,
}


/// Extended row from `oc_filecache_extended` (authoritative for times, REQ §9.4).
#[derive(Debug, Clone, Default)]
pub struct FileCacheExtRow {
    pub metadata_etag: Option<String>,
    pub creation_time: i64,
    pub upload_time: i64,
}
