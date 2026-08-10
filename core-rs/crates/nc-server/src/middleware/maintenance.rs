use axum::{
    body::Body,
    http::{Response, StatusCode},
};

use crate::state::AppState;

/// Maintenance-mode check (Phase 18.6).
///
/// When `maintenance = true` in `config/config.php` (read at startup into
/// `NcConfig`), every route **except** `/status.php` and `/heartbeat` returns
/// `Some(503)` with:
///   X-Nextcloud-Maintenance-Mode: 1
///   Retry-After: 120
///
/// OCS routes (`/ocs/`) get an OCS-envelope body; all other routes get
/// plain text.  PHP writes `maintenance` via `SystemConfig::setValue()` to
/// `config.php` — never to `oc_appconfig` — so checking `NcConfig` is correct.
/// Toggling maintenance mode while the server is running requires a restart.
///
/// Extracted from the former `from_fn` middleware so the three request
/// middleware layers (static → maintenance → auth) run inside one composite
/// layer (`router::http_middleware_stack`) instead of three — ~2 fewer
/// wrapper polls per await per request.  `None` falls through to auth.
pub async fn maintenance_check(state: &AppState, path: &str) -> Option<Response<Body>> {
    // These two endpoints are always served regardless of maintenance mode.
    if path == "/status.php" || path == "/heartbeat" {
        return None;
    }

    if !state.nc_config.maintenance {
        return None;
    }

    // Determine response format based on route prefix.
    let is_ocs = path.starts_with("/ocs/");

    let body_text = if is_ocs {
        // Minimal OCS XML envelope for maintenance mode (v1 and v2 both accept this).
        "<?xml version=\"1.0\"?>\n\
         <ocs>\n\
           <meta>\n\
             <status>failure</status>\n\
             <statuscode>503</statuscode>\n\
             <message>Server is in maintenance mode</message>\n\
           </meta>\n\
           <data/>\n\
         </ocs>"
            .to_string()
    } else {
        "Server is in maintenance mode.".to_string()
    };

    let content_type = if is_ocs {
        "text/xml; charset=UTF-8"
    } else {
        "text/plain; charset=UTF-8"
    };

    Some(
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("Content-Type", content_type)
            .header("X-Nextcloud-Maintenance-Mode", "1")
            .header("Retry-After", "120")
            .body(Body::from(body_text))
            .expect("maintenance response is well-formed"),
    )
}
