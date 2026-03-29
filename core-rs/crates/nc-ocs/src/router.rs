use axum::{extract::FromRef, routing::get, Router};

use crate::{
    handlers::{ocs_capabilities, ocs_config},
    OcsState,
};

/// Build the OCS sub-router.
///
/// The router is generic over the outer application state `S`.  Callers must
/// ensure `OcsState: FromRef<S>` so axum can extract `OcsState` automatically
/// from the outer state when dispatching to these handlers.
///
/// Do **not** call `.with_state()` here; the outer router does it once for the
/// combined state, which is the correct axum 0.8 pattern for nested routers.
pub fn ocs_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    OcsState: FromRef<S>,
{
    Router::new()
        // /config on both versions
        .route("/ocs/v1.php/config", get(ocs_config))
        .route("/ocs/v2.php/config", get(ocs_config))
        // /cloud/capabilities on both versions
        .route("/ocs/v1.php/cloud/capabilities", get(ocs_capabilities))
        .route("/ocs/v2.php/cloud/capabilities", get(ocs_capabilities))
}
