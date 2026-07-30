use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::state::AppState;

/// `GET /status.php`
///
/// Always served — even in maintenance mode (middleware skips this path).
/// Returns JSON with all fields from REQ §3.
///
/// `installed` and `maintenance` are read from `NcConfig` (parsed from
/// `config/config.php` at startup — PHP writes both via `SystemConfig`, not
/// `oc_appconfig`).  All other fields come from the in-memory
/// `AppConfigCache` — no DB query per request.
pub async fn status(State(state): State<AppState>) -> Response {
    let ac = state
        .appconfig_cache
        .read()
        .expect("appconfig lock poisoned");

    // PHP's `status.php` reads `version` from `version.php` (a static file at the
    // Nextcloud root containing `$OC_Version = [34, 0, 1, 2]` and
    // `$OC_VersionString = '34.0.1'`).  We already parse the dotted version from
    // `config.php` ($CONFIG['version']).  Derive the human-readable string by
    // dropping the last component — `34.0.1.2` → `34.0.1`.
    let version = state.nc_config.version.clone()
        .or_else(|| ac.get_string("core", "oc_version"))
        .or_else(|| ac.get_string("core", "version"))
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let version_string = version.rfind('.')
        .map(|pos| version[..pos].to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());

    let body = StatusResponse {
        installed: state.nc_config.installed,
        maintenance: state.nc_config.maintenance,
        needs_db_upgrade: ac.get_bool("core", "needsDbUpgrade"),
        version,
        version_string,
        edition: String::new(),
        product_name: "Nextcloud".to_string(),
        extended_support: false,
    };

    // Drop the read guard before building the response.
    drop(ac);

    // REQ §3: Content-Type: application/json, Access-Control-Allow-Origin: *
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        Json(body),
    )
        .into_response()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    installed: bool,
    maintenance: bool,
    needs_db_upgrade: bool,
    version: String,
    /// PHP outputs `versionstring` (all lowercase, not camelCase).
    #[serde(rename = "versionstring")]
    version_string: String,
    edition: String,
    /// PHP outputs `productname` (all lowercase, not camelCase).
    #[serde(rename = "productname")]
    product_name: String,
    extended_support: bool,
}
