#![forbid(unsafe_code)]

pub mod archive;
pub(crate) mod archive_stream;
pub mod bulk_handler;
pub mod davfile;
pub mod filesystem;
pub mod handler;
pub mod locksystem;
pub mod metadata;
pub(crate) mod mtime;
pub mod preview;
pub mod propagator;
pub mod props;
pub mod quota;
pub mod row;
pub(crate) mod tags;
pub mod upload;
pub mod upload_handler;
pub(crate) mod versions;

pub use bulk_handler::bulk_handler;
pub use filesystem::NcFileSystem;
pub use handler::dav_handler;
pub use locksystem::NcLockSystem;
pub use metadata::NcMetaData;
pub use preview::ProviderRegistry;
pub use upload::{SharedUploadStateStore, UploadMetadata, UploadStateStore};
pub use upload_handler::upload_handler;

use nc_db::{
    appconfig::SharedAppConfigCache, filename_validator::SharedFilenameValidator,
    mime::SharedMimeCache, pool::DbPool,
};
use std::{path::PathBuf, sync::Arc};

// ─── Write-result channel ─────────────────────────────────────────────────────

/// Written by `NcDavFile::flush()` so that `dav_handler` can inject
/// Nextcloud-specific response headers (`OC-FileId`, `X-OC-MTime: accepted`, …).
pub struct WriteResult {
    pub fileid: i64,
    pub etag: String,
    pub mtime_accepted: bool,
    pub ctime_accepted: bool,
}

/// Shared per-request container for the write result.
pub type SharedWriteResult = Arc<std::sync::Mutex<Option<WriteResult>>>;

// ─── Put-error channel ────────────────────────────────────────────────────────

/// The cause of a failed PUT, set by `NcDavFile::flush()` before it returns a
/// `FsError`.  `dav_handler` reads this after the response is assembled and
/// rewrites the HTTP status code appropriately (e.g. `GeneralFailure` 500 →
/// `BadRequest` 400 for a checksum mismatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutErrorKind {
    ChecksumMismatch,
}

pub type SharedPutError = Arc<std::sync::Mutex<Option<PutErrorKind>>>;

// ─── Shared state ─────────────────────────────────────────────────────────────

/// State shared across all DAV requests.
///
/// All fields are cheap to clone — `Arc`-wrapped internally.
#[derive(Clone)]
pub struct NcDavState {
    pub pool: DbPool,
    pub mime_cache: SharedMimeCache,
    /// App config cache — used for `{oc:}data-fingerprint` and future config reads.
    pub appconfig_cache: SharedAppConfigCache,
    pub table_prefix: String,
    pub data_directory: PathBuf,
    /// Instance ID for `{oc:}id` (e.g. `"oc3a7f…"`). Read from
    /// `oc_appconfig` key `core/instanceid` at startup.
    pub instance_id: Arc<String>,
    /// Filename validator built from `config.php`.  Checked before every
    /// write operation (PUT, MKCOL, MOVE destination, COPY destination)
    /// to enforce forbidden filename rules (§5.1).
    pub filename_validator: SharedFilenameValidator,
    /// Base URL from `overwrite.cli.url` config key.  Used to generate
    /// `{oc:}downloadURL` for home-storage files (PHASE-7.6).
    /// Empty string when not configured.
    pub base_url: Arc<String>,
    /// In-process store for chunked upload v2 metadata.
    /// Used when no distributed cache is configured (PHASE-5.5).
    pub upload_state_store: SharedUploadStateStore,
    // ── Preview / thumbnail (§10.12 / §11.1) ───────────────────────────────
    /// Resolved preview-provider gating, built once at startup from system config.
    /// Single source of truth for `{nc:}has-preview` and (11.4) native generation —
    /// replaces the old `enable_previews` / `preview_ffmpeg_path` /
    /// `preview_libreoffice_path` fields.
    pub preview_registry: Arc<ProviderRegistry>,
}
