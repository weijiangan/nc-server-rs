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

    let body = StatusResponse {
        installed: state.nc_config.installed,
        maintenance: state.nc_config.maintenance,
        needs_db_upgrade: ac.get_bool("core", "needsDbUpgrade"),
        version: ac
            .get_string("core", "oc_version")
            .or_else(|| ac.get_string("core", "version"))
            .unwrap_or_else(|| "0.0.0.0".to_string()),
        version_string: ac
            .get_string("core", "oc_version_string")
            .or_else(|| ac.get_string("core", "versionstring"))
            .unwrap_or_else(|| "Unknown".to_string()),
        edition: ac.get_string("core", "edition").unwrap_or_default(),
        product_name: ac
            .get_string("core", "productname")
            .unwrap_or_else(|| "Nextcloud".to_string()),
        extended_support: ac.get_bool("core", "extendedSupport"),
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
    version_string: String,
    edition: String,
    product_name: String,
    extended_support: bool,
}
