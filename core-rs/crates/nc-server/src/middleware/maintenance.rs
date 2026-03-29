use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    middleware::Next,
};

use crate::state::AppState;

/// Maintenance-mode middleware.
///
/// When `core.maintenance = true` in `oc_appconfig`, every route **except**
/// `/status.php` and `/heartbeat` returns:
///   HTTP 503
///   X-Nextcloud-Maintenance-Mode: 1
///   Retry-After: 120
///
/// OCS routes (`/ocs/`) get an OCS-envelope body; all other routes get
/// plain text.  The check is a single `RwLock::read()` on the in-memory
/// cache — no DB query per request.
pub async fn maintenance_guard(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    let path = request.uri().path().to_owned();

    // These two endpoints are always served regardless of maintenance mode.
    if path == "/status.php" || path == "/heartbeat" {
        return next.run(request).await;
    }

    let is_maintenance = state
        .appconfig_cache
        .read()
        .expect("appconfig lock poisoned")
        .is_maintenance();

    if !is_maintenance {
        return next.run(request).await;
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

    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("Content-Type", content_type)
        .header("X-Nextcloud-Maintenance-Mode", "1")
        .header("Retry-After", "120")
        .body(Body::from(body_text))
        .expect("maintenance response is well-formed")
}
