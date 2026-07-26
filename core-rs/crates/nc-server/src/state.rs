use axum::extract::FromRef;
use nc_db::{
    appconfig::SharedAppConfigCache, config::NcConfig, filename_validator::FilenameValidator,
    mime::SharedMimeCache, pool::DbPool,
};
use nc_ocs::SharedCapabilityCache;
use std::sync::Arc;

/// Application state shared across all axum handlers via `State<AppState>`.
///
/// All fields are cheap to clone (`Arc` internally) so the derive is fine.
#[derive(Clone)]
pub struct AppState {
    pub pool: DbPool,
    pub mime_cache: SharedMimeCache,
    pub appconfig_cache: SharedAppConfigCache,
    pub capability_cache: SharedCapabilityCache,
    pub token_cache: nc_auth::SharedTokenCache,
    /// Parsed `config/config.php` — system-level config (e.g. brute-force flag).
    pub nc_config: Arc<NcConfig>,
    /// Nextcloud installation root (parent of `config/`, `apps/`, etc.)
    pub nc_root: std::path::PathBuf,
    /// Table prefix (default `oc_`)
    pub table_prefix: String,
    /// PHP-FPM FastCGI proxy state.  `None` when `fastcgi_socket` is not set
    /// in `config.php`; in that case FastCGI-bound routes return `502`.
    pub fastcgi: Option<nc_fastcgi::FastCgiState>,
    /// The Nextcloud instance ID (value of `instanceid` in `config.php`).
    ///
    /// PHP's `session_name()` is set to this value, making it the name of the
    /// PHP session cookie (e.g. `oc1a2b3c4d5e`).  The auth middleware uses it
    /// to locate the correct cookie when no `Authorization` header is present
    /// (§7.9.6).
    ///
    /// Always present on an installed instance (`OC_Util::getInstanceId()`
    /// auto-generates and persists the value).  Defaults to `""` for
    /// pre-install / test states where `config.php` may be absent or partial.
    pub instanceid: String,
    /// In-process session-identity cache (§7.9.5).
    ///
    /// Keyed on `SHA-256(php_session_cookie_value)` → `(SessionIdentity, Instant)`.
    /// TTL = 60 seconds.  Entries are evicted lazily on lookup and periodically
    /// by the background eviction task spawned in `main.rs`.
    ///
    /// `None` when PHP-FPM is not configured (`fastcgi` is `None`).  When
    /// PHP-FPM is absent there is no session resolver to call, so no cache is
    /// needed.
    pub session_cache: Option<nc_auth::SharedSessionCache>,
    /// In-process store for chunked upload v2 metadata (PHASE-5.5).
    /// Created regardless of distributed cache configuration - serves as the
    /// fallback store when Redis/Memcached is not available.
    pub upload_state_store: nc_dav::SharedUploadStateStore,
    /// Resolved preview-provider gating (§11.1), built once at startup from
    /// `config.php`.  Shared with the DAV layer for `{nc:}has-preview` and (11.4)
    /// native generation.
    pub preview_registry: Arc<nc_dav::ProviderRegistry>,
    /// Native preview-generation service (§11.4/11.5): the Imaginary backend +
    /// snowflake generator + admission semaphore + in-flight coalescer.  Its backend
    /// is `None` when Imaginary is not configured+gated, in which case generation
    /// misses proxy to PHP-FPM (hit-serving stays on regardless).
    pub preview_gen: Arc<crate::preview_gen::PreviewGen>,
}

/// Allow axum to extract `OcsState` from the top-level `AppState` using the
/// `FromRef` pattern.  This lets `ocs_router()` be generic over `AppState`
/// without creating a circular crate dependency.
impl FromRef<AppState> for nc_ocs::OcsState {
    fn from_ref(state: &AppState) -> Self {
        nc_ocs::OcsState {
            appconfig_cache: state.appconfig_cache.clone(),
            capability_cache: state.capability_cache.clone(),
        }
    }
}

/// Allow axum to extract `NcDavState` from `AppState` for DAV handlers.
impl FromRef<AppState> for nc_dav::NcDavState {
    fn from_ref(state: &AppState) -> Self {
        let data_directory = state
            .nc_config
            .datadirectory
            .clone()
            .unwrap_or_else(|| state.nc_root.join("data"));

        let instance_id = Arc::new(state.nc_config.instanceid.clone().unwrap_or_default());

        let filename_validator = Arc::new(FilenameValidator::from_config(&state.nc_config));

        let base_url = Arc::new(
            state
                .nc_config
                .overwrite_cli_url
                .clone()
                .unwrap_or_default(),
        );

        nc_dav::NcDavState {
            pool: state.pool.clone(),
            mime_cache: state.mime_cache.clone(),
            appconfig_cache: state.appconfig_cache.clone(),
            table_prefix: state.table_prefix.clone(),
            data_directory,
            instance_id,
            filename_validator,
            base_url,
            upload_state_store: state.upload_state_store.clone(),
            preview_registry: state.preview_registry.clone(),
        }
    }
}
