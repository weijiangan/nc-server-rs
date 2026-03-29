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

        let filename_validator =
            Arc::new(FilenameValidator::from_config(&state.nc_config));

        let base_url = Arc::new(
            state.nc_config.overwrite_cli_url.clone().unwrap_or_default(),
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
        }
    }
}
