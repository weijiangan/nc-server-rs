//! Native generation orchestration — the **miss path** that ties the Imaginary
//! backend (11.4), the concurrency primitives (11.3) and persistence (11.5) together.
//!
//! On a cache miss, when the source is **Rust-generatable** (Imaginary configured
//! *and* gated in via `enabledPreviewProviders`), the requested variant is generated
//! natively using the **max-first** model and persisted; the served row is then
//! indistinguishable from a PHP-generated one (full bidirectional interop, §11.5
//! column/byte parity).  When the source is not generatable, or **any** step fails,
//! the caller proxies the original request to PHP-FPM — so behaviour is never worse
//! than today and the proven hit-serving path (11.2) is untouched.
//!
//! ## Max-first flow (`Generator::generatePreviews`)
//!
//! 1. Ensure the **max** preview exists (generate from the raw source at
//!    `preview_max_x/y` if not) — coalesced + semaphore-gated, then persisted.
//! 2. Bucket the requested size relative to the max's actual dims (`calculateSize`).
//! 3. Serve the max if it matches; else ensure the **derived** variant (re-submit the
//!    max bytes — never re-decode the source) — coalesced + gated, then persisted.
//!
//! Concurrency: one tokio semaphore permit (`preview_concurrency_new`) is held across
//! each Imaginary call, and duplicate in-flight requests for the same post-bucketing
//! key coalesce onto a single generation (`Coalescer`) that runs to completion even
//! if every client disconnects (warming the cache).

use crate::state::AppState;
use bytes::Bytes;
use nc_dav::row::FileCacheRow;
use nc_db::now_secs;
use nc_preview::backend::{
    BackendError, ImaginaryClient, PreviewBackend, DEFAULT_MAX_FILESIZE_MIB,
};
use nc_preview::concurrency::{generation_semaphore, CoalesceKey, Coalescer};
use nc_preview::persist::{self, NewPreview};
use nc_preview::snowflake::SnowflakeGenerator;
use nc_preview::store::{self, PreviewRow};
use std::sync::Arc;

/// A generation failure.  Every variant means "fall back to PHP-FPM" — the caller
/// proxies the original request, which may still succeed via PHP's GD/imagick/other
/// providers.  Logged at `info!` (an expected, recoverable fallback — not an error).
#[derive(thiserror::Error, Debug)]
pub enum GenError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Persist(#[from] persist::PersistError),
    #[error("source/max preview bytes unreadable: {0}")]
    Io(String),
    #[error("output mimetype could not be resolved: {0}")]
    Mime(String),
    #[error("generation semaphore unavailable")]
    Semaphore,
}

/// The generation service: holds the (optional) Imaginary backend, the admission
/// semaphore, the in-flight coalescer and the snowflake generator.  Built once at
/// startup and shared behind an `Arc` via [`AppState`].
pub struct PreviewGen {
    /// `None` when Imaginary is not configured + gated — generation then always
    /// falls back to PHP-FPM (hit-serving stays on).
    backend: Option<Arc<ImaginaryClient>>,
    semaphore: Arc<tokio::sync::Semaphore>,
    coalescer: Coalescer<PreviewRow, GenError>,
    snowflake: Arc<SnowflakeGenerator>,
    /// `preview_max_x` / `preview_max_y` (default 4096 each).
    max_w: u32,
    max_h: u32,
}

impl PreviewGen {
    /// Build from system config + the appconfig cache (for the Imaginary quality
    /// settings) + the resolved provider registry (for gating).
    pub fn from_config(
        cfg: &nc_db::config::NcConfig,
        appconfig: &nc_db::appconfig::SharedAppConfigCache,
        registry: &nc_dav::ProviderRegistry,
    ) -> Arc<Self> {
        let backend = if registry.imaginary_available() {
            let (jpeg_q, webp_q) = {
                let ac = appconfig.read().expect("appconfig cache lock");
                (
                    ac.get_string("preview", "jpeg_quality")
                        .unwrap_or_else(|| "80".to_string()),
                    ac.get_string("preview", "webp_quality")
                        .unwrap_or_else(|| "80".to_string()),
                )
            };
            cfg.preview_imaginary_url
                .as_ref()
                .map(|u| u.expose().to_string())
                .and_then(|url| {
                    ImaginaryClient::new(
                        &url,
                        cfg.preview_imaginary_key
                            .as_ref()
                            .map(|s| s.expose())
                            .unwrap_or(""),
                        cfg.preview_format.clone(),
                        jpeg_q,
                        webp_q,
                        cfg.preview_max_filesize_image
                            .unwrap_or(DEFAULT_MAX_FILESIZE_MIB),
                    )
                })
                .map(Arc::new)
        } else {
            None
        };

        Arc::new(Self {
            semaphore: generation_semaphore(cfg.preview_concurrency_new),
            coalescer: Coalescer::new(),
            snowflake: Arc::new(SnowflakeGenerator::from_config(cfg.serverid)),
            max_w: cfg.preview_max_x.unwrap_or(4096).max(1) as u32,
            max_h: cfg.preview_max_y.unwrap_or(4096).max(1) as u32,
            backend,
        })
    }

    /// Whether native generation is possible at all (Imaginary configured + gated).
    /// The handler uses this, together with `rust_generatable(mime)`, to decide
    /// between generating and proxying on a miss.
    pub fn backend_available(&self) -> bool {
        self.backend.is_some()
    }

    /// Ensure the **max** preview for `fc_row` exists, generating + persisting it on
    /// a miss.  Returns the max row, or `None` (→ the caller proxies to PHP-FPM).
    pub async fn ensure_max(
        &self,
        state: &AppState,
        uid: &str,
        fc_row: &FileCacheRow,
        source_mime: &str,
    ) -> Option<PreviewRow> {
        let file_id = fc_row.fileid;
        // A concurrent request may have generated it while we decided to.
        let rows = store::load_preview_rows(&state.pool, &state.table_prefix, file_id).await;
        if let Some(m) = store::find_max(&rows, -1) {
            return Some(m.clone());
        }
        let backend = self.backend.as_ref()?.clone();
        let key: CoalesceKey = (file_id, self.max_w, self.max_h, false, -1);

        let semaphore = self.semaphore.clone();
        let snowflake = self.snowflake.clone();
        let st = state.clone();
        let uid = uid.to_string();
        let fc_row = fc_row.clone();
        let source_mime = source_mime.to_string();
        let (max_w, max_h) = (self.max_w, self.max_h);

        let result = self
            .coalescer
            .run(key, move || async move {
                let _permit = semaphore.acquire().await.map_err(|_| GenError::Semaphore)?;
                let datadir = data_dir(&st);
                let fc_path = fc_row
                    .path
                    .clone()
                    .ok_or_else(|| GenError::Io("no filecache path".into()))?;
                let src_path = nc_dav::row::disk_path(&datadir, &uid, &fc_path);
                let bytes = tokio::fs::read(&src_path)
                    .await
                    .map_err(|e| GenError::Io(e.to_string()))?;
                let gen = backend
                    .generate_max(Bytes::from(bytes), &source_mime, max_w, max_h)
                    .await?;
                let out_id = resolve_mime_id(&st, &gen.output_mime).await?;
                let name =
                    store::preview_name(-1, gen.width, gen.height, false, true, &gen.output_mime);
                let id = snowflake.next_id();
                persist::write_preview_bytes(
                    &datadir,
                    &st.instanceid,
                    file_id,
                    &name,
                    &gen.bytes,
                    id,
                )
                .await
                .map_err(|e| GenError::Io(e.to_string()))?;
                let np = NewPreview {
                    file_id,
                    storage_id: fc_row.storage,
                    width: gen.width,
                    height: gen.height,
                    mimetype_id: out_id,
                    source_mimetype_id: fc_row.mimetype as i32,
                    max: true,
                    cropped: false,
                    etag: fc_row.etag.clone().unwrap_or_default(),
                    mtime: now_secs(),
                    size: gen.bytes.len() as i64,
                    version_id: -1,
                };
                Ok(persist::insert_preview(&st.pool, &st.table_prefix, &np, id).await?)
            })
            .await;

        log_result("max", file_id, &result);
        result.ok().map(|r| (*r).clone())
    }

    /// Ensure the **derived** variant `(bw, bh, crop)` for `fc_row` exists, generating
    /// it from the max preview's bytes on a miss.  Returns the variant row, or `None`
    /// (→ proxy).
    #[allow(clippy::too_many_arguments)]
    pub async fn ensure_derived(
        &self,
        state: &AppState,
        _uid: &str,
        fc_row: &FileCacheRow,
        source_mime: &str,
        max_row: &PreviewRow,
        bw: u32,
        bh: u32,
        crop: bool,
    ) -> Option<PreviewRow> {
        let file_id = fc_row.fileid;
        let rows = store::load_preview_rows(&state.pool, &state.table_prefix, file_id).await;
        if let Some(r) = store::find_match(&rows, bw, bh, crop, max_row.mimetype_id, -1) {
            return Some(r.clone());
        }
        let backend = self.backend.as_ref()?.clone();
        let key: CoalesceKey = (file_id, bw, bh, crop, -1);

        // Resolve the max preview's output mimetype string (to read its bytes).  The
        // cache guard is a statement-scoped temporary — not held across any await.
        let max_mime = state
            .mime_cache
            .read()
            .expect("mime cache lock")
            .get_name(max_row.mimetype_id as i64)?
            .to_string();
        // Owned copies of the max row's name components (the closure is 'static).
        // The max preview's bytes live at a path derived from file_id (not uid).
        let max_version = max_row.version_id;
        let max_px_w = max_row.width;
        let max_px_h = max_row.height;
        let max_cropped = max_row.cropped;
        let max_is_max = max_row.max;

        let semaphore = self.semaphore.clone();
        let snowflake = self.snowflake.clone();
        let st = state.clone();
        let fc_row = fc_row.clone();
        let source_mime = source_mime.to_string();

        let result = self
            .coalescer
            .run(key, move || async move {
                let _permit = semaphore.acquire().await.map_err(|_| GenError::Semaphore)?;
                let datadir = data_dir(&st);
                let max_name = store::preview_name(
                    max_version,
                    max_px_w,
                    max_px_h,
                    max_cropped,
                    max_is_max,
                    &max_mime,
                );
                let max_path =
                    store::preview_byte_path(&datadir, &st.instanceid, file_id, &max_name);
                let max_bytes = tokio::fs::read(&max_path)
                    .await
                    .map_err(|e| GenError::Io(e.to_string()))?;
                let gen = backend
                    .render_from_max(Bytes::from(max_bytes), &source_mime, bw, bh, crop)
                    .await?;
                let out_id = resolve_mime_id(&st, &gen.output_mime).await?;
                let name = store::preview_name(-1, bw, bh, crop, false, &gen.output_mime);
                let id = snowflake.next_id();
                persist::write_preview_bytes(
                    &datadir,
                    &st.instanceid,
                    file_id,
                    &name,
                    &gen.bytes,
                    id,
                )
                .await
                .map_err(|e| GenError::Io(e.to_string()))?;
                let np = NewPreview {
                    file_id,
                    storage_id: fc_row.storage,
                    width: bw,
                    height: bh,
                    mimetype_id: out_id,
                    source_mimetype_id: fc_row.mimetype as i32,
                    max: false,
                    cropped: crop,
                    etag: fc_row.etag.clone().unwrap_or_default(),
                    mtime: now_secs(),
                    size: gen.bytes.len() as i64,
                    version_id: -1,
                };
                Ok(persist::insert_preview(&st.pool, &st.table_prefix, &np, id).await?)
            })
            .await;

        log_result("derived", file_id, &result);
        result.ok().map(|r| (*r).clone())
    }
}

/// Log a generation outcome.  Failures are `info!` — an expected fallback to
/// PHP-FPM, not a server error (the request still succeeds via the proxy).
fn log_result(what: &str, file_id: i64, result: &Result<Arc<PreviewRow>, Arc<GenError>>) {
    match result {
        Ok(row) => tracing::info!(
            file_id,
            preview_id = row.id,
            "native {what} preview generated"
        ),
        Err(e) => {
            tracing::info!(file_id, error = %e, "native {what} generation failed; falling back to PHP-FPM")
        }
    }
}

/// Resolve an output mimetype string to its `oc_mimetypes` id (auto-inserting if
/// needed — PHP `IMimeTypeLoader::getId`).
async fn resolve_mime_id(state: &AppState, mime: &str) -> Result<i32, GenError> {
    let id = nc_db::mime::get_or_insert_mime_id(
        &state.pool,
        &state.table_prefix,
        &state.mime_cache,
        mime,
    )
    .await;
    if id <= 0 {
        return Err(GenError::Mime(mime.to_string()));
    }
    Ok(id as i32)
}

fn data_dir(state: &AppState) -> std::path::PathBuf {
    state
        .nc_config
        .datadirectory
        .clone()
        .unwrap_or_else(|| state.nc_root.join("data"))
}
