use axum::http::StatusCode;

/// `GET /heartbeat`
///
/// No auth required. Returns 200 OK with an empty body.
/// Skipped by the maintenance-mode middleware.
pub async fn heartbeat() -> StatusCode {
    StatusCode::OK
}
