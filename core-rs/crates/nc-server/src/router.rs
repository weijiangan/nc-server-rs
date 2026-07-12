use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tower::ServiceExt as _;
use tower_http::services::ServeDir;

use crate::{
    handlers::{heartbeat::heartbeat, status::status},
    middleware::{auth::auth_layer, maintenance::maintenance_guard},
    state::AppState,
};

/// Outermost middleware: serve physical files directly from the Nextcloud root
/// before the request reaches any route handler.
///
/// Mirrors nginx's `try_files $uri @php` directive:
/// - Only GET / HEAD requests are candidates.
/// - Paths containing `.php` are always passed through (PHP-FPM handles them).
/// - Path traversal (`..`) is rejected immediately.
/// - If the resolved path is a regular file, it is served by `tower_http`'s
///   `ServeDir` (which handles ETag, Last-Modified, Range, and Content-Type
///   detection automatically).
/// - Everything else falls through to the auth + route layers.
///
/// This covers `/core/`, `/dist/`, `/themes/`, and app-level static assets
/// such as `/apps/files/img/icon.svg` that the PHP route wildcards would
/// otherwise swallow and send to PHP-FPM.
async fn try_static_files(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if matches!(req.method(), &Method::GET | &Method::HEAD) {
        let path = req.uri().path();
        // Skip PHP scripts and any path that looks like traversal.
        if !path.contains(".php") && !path.contains("..") {
            let candidate = state.nc_root.join(path.trim_start_matches('/'));
            if tokio::fs::metadata(&candidate)
                .await
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                return match ServeDir::new(&state.nc_root).oneshot(req).await {
                    Ok(resp) => resp.into_response(),
                    Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                };
            }
        }
    }
    next.run(req).await
}

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
        //
        // Each mount point needs three routes because axum's `{*path}` catch-all
        // requires at least one character — it does NOT match a bare trailing
        // slash.  WebDAV clients routinely PROPFIND the collection root with a
        // trailing slash (e.g. `PROPFIND /remote.php/webdav/`), so we register:
        //   /mount          — exact, no trailing slash
        //   /mount/         — exact, trailing slash only (collection root)
        //   /mount/{*path}  — one or more path segments
        .route(
            "/remote.php/webdav",
            axum::routing::any(nc_dav::dav_handler),
        )
        .route(
            "/remote.php/webdav/",
            axum::routing::any(nc_dav::dav_handler),
        )
        .route(
            "/remote.php/webdav/{*path}",
            axum::routing::any(nc_dav::dav_handler),
        )
        .route("/remote.php/dav", axum::routing::any(nc_dav::dav_handler))
        .route("/remote.php/dav/", axum::routing::any(nc_dav::dav_handler))
        // Non-files DAV sub-trees are served by PHP/SabreDAV.  These more-specific
        // routes must come before the generic `/remote.php/dav/{*path}` wildcard so
        // that axum matches them first.
        .route("/remote.php/dav/versions/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/comments/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/trashbin/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/uploads/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/principals/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/calendars/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/public-calendars/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/system-calendars/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/addressbooks/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/avatars/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/remote.php/dav/access-control/{*path}", axum::routing::any(php_fpm_fallback))
        // Native file-storage handler for /remote.php/dav/files/{uid}/{*path}
        .route(
            "/remote.php/dav/{*path}",
            axum::routing::any(nc_dav::dav_handler),
        )
        .route("/dav", axum::routing::any(nc_dav::dav_handler))
        .route("/dav/", axum::routing::any(nc_dav::dav_handler))
        .route("/dav/versions/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/comments/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/trashbin/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/uploads/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/principals/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/calendars/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/public-calendars/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/system-calendars/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/addressbooks/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/avatars/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/dav/{*path}", axum::routing::any(nc_dav::dav_handler))
        // Static PHP-FPM routes — always forwarded regardless of registry
        .route("/public.php/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/.well-known/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/login/{*path}", axum::routing::any(php_fpm_fallback))
        .route("/index.php", axum::routing::any(php_fpm_fallback))
        .route("/index.php/{*path}", axum::routing::any(php_fpm_fallback))
        // Root path — Nextcloud redirects to default page or login
        .route("/", axum::routing::any(php_fpm_fallback));

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
    //
    // Request processing order (outer → inner):
    //   try_static_files  — serve physical files before routing; bypasses auth
    //                       (JS/CSS/images are always public)
    //   maintenance_guard — reject API calls when maintenance mode is on
    //   auth_layer        — validate bearer / session token
    //   routes            — native handlers + PHP-FPM proxy
    r.layer(middleware::from_fn_with_state(state.clone(), auth_layer))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            maintenance_guard,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            try_static_files,
        ))
        .with_state(state)
}
