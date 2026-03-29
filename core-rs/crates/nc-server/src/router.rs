use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use crate::{
    handlers::{heartbeat::heartbeat, status::status},
    middleware::{auth::auth_layer, maintenance::maintenance_guard},
    state::AppState,
};

/// Fallback handler for PHP-FPM-bound routes.
///
/// When `state.fastcgi` is `Some`, delegates to `nc_fastcgi::proxy_handler`
/// (Phase 7.1 will implement the real FastCGI client).
/// When `None` (PHP-FPM not configured), returns `502 Bad Gateway`.
async fn php_fpm_fallback(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request<Body>,
) -> Response {
    match state.fastcgi.as_ref() {
        Some(fpm) => nc_fastcgi::proxy_handler(fpm, req).await,
        None => (
            StatusCode::BAD_GATEWAY,
            "PHP-FPM not configured (set 'fastcgi_socket' in config.php)\n",
        )
            .into_response(),
    }
}

/// Build the axum router.
///
/// `php_routes` is the list of URL prefixes produced by
/// [`nc_fastcgi::build_route_registry`] at startup.  Each entry is registered
/// as both an exact match (`/base`) and a wildcard prefix (`/base/{*tail}`),
/// both forwarded to PHP-FPM via `php_fpm_fallback`.
///
/// This replaces the previous static `/apps/{*path}` catch-all with explicit
/// per-app entries so that truly unknown paths return `404 Not Found` rather
/// than being dispatched blindly to PHP-FPM.  Routes not matched by any
/// registered entry get axum's built-in 404 response.
pub fn build(state: AppState, php_routes: Vec<nc_fastcgi::RouteEntry>) -> Router {
    // ── Static native handlers ───────────────────────────────────────────────
    let r = Router::new()
        // Always-on endpoints
        .route("/status.php", get(status))
        .route("/heartbeat", get(heartbeat))
        // OCS native handlers (Phase 2) — must be merged before the OCS catch-all.
        .merge(nc_ocs::router::ocs_router::<AppState>())
        // OCS catch-all: unimplemented OCS endpoints → PHP-FPM
        .route("/ocs/v1.php/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/ocs/v2.php/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/ocs-provider/index.php", get(php_fpm_fallback))
        // DAV (Phase 4) — native handlers; all mount points share the same
        // handler which resolves the strip prefix from the request path.
        .route("/remote.php/webdav",         axum::routing::any(nc_dav::dav_handler))
        .route("/remote.php/webdav/{*path}", axum::routing::any(nc_dav::dav_handler))
        .route("/remote.php/dav",            axum::routing::any(nc_dav::dav_handler))
        .route("/remote.php/dav/{*path}",    axum::routing::any(nc_dav::dav_handler))
        .route("/dav",                       axum::routing::any(nc_dav::dav_handler))
        .route("/dav/{*path}",               axum::routing::any(nc_dav::dav_handler))
        // Static PHP-FPM routes — always forwarded regardless of registry
        .route("/public.php/{*path}",        axum::routing::any(php_fpm_fallback))
        .route("/.well-known/{*path}",       axum::routing::any(php_fpm_fallback))
        .route("/login/{*path}",             axum::routing::any(php_fpm_fallback))
        .route("/index.php",                 axum::routing::any(php_fpm_fallback))
        .route("/index.php/{*path}",         axum::routing::any(php_fpm_fallback));

    // ── Registry-built PHP-FPM routes (Phase 7.5) ───────────────────────────
    //
    // For each entry from `build_route_registry` we register two axum routes:
    //   /base          — exact match (e.g. GET /settings, GET /s)
    //   /base/{*tail}  — wildcard prefix (e.g. GET /s/abc123, POST /settings/admin/foo)
    //
    // Axum matches the most-specific registered route first, so native handlers
    // registered above always take precedence over these PHP-FPM fallbacks.
    //
    // Routes not matched by any entry (native or PHP-FPM) return axum's default
    // 404 Not Found — there is no `/apps/{*path}` catch-all.
    let r = php_routes.iter().fold(r, |router, entry| {
        let base = entry.base.trim_end_matches('/');
        let prefix_pattern = format!("{}/{{*tail}}", base);
        router
            .route(&prefix_pattern, axum::routing::any(php_fpm_fallback))
            .route(base, axum::routing::any(php_fpm_fallback))
    });

    // ── Middleware (outermost layers are added last) ─────────────────────────
    r.layer(middleware::from_fn_with_state(state.clone(), auth_layer))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            maintenance_guard,
        ))
        .with_state(state)
}
