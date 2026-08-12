use axum::{
    body::Body,
    extract::{FromRef, State},
    http::{Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tower::ServiceExt as _;
use tower_http::services::ServeDir;
use tower_http::trace::{self, TraceLayer};
use tracing::Level;

use crate::{
    handlers::{heartbeat::heartbeat, status::status},
    middleware::{auth::auth_check, maintenance::maintenance_check},
    state::AppState,
};

/// Static-file serving check (Phase 18.6): returns `Some(response)` when the
/// path is a whitelisted static asset that exists on disk, `None` to fall
/// through to the maintenance + auth + route stack.  Extracted from the
/// former `from_fn` middleware so the three request-middleware layers
/// (static → maintenance → auth) run inside one composite
/// (`http_middleware_stack`) instead of three — ~2 fewer wrapper polls per
/// await per request.
///
/// Mirrors nginx's `try_files $uri @php` directive:
/// - Only GET / HEAD requests are candidates.
/// - Paths containing `.php` are always passed through (PHP-FPM handles them).
/// - Path traversal (`..`) is rejected immediately.
/// - If the resolved path is a regular file, it is served by `tower_http`'s
///   `ServeDir` (which handles ETag, Last-Modified, Range, and Content-Type
///   detection automatically).
///
/// Covers `/core/`, `/dist/`, `/themes/`, and app-level static assets such as
/// `/apps/files/img/icon.svg` that the PHP route wildcards would otherwise
/// swallow and send to PHP-FPM.
async fn try_static_files_check(state: &AppState, req: &mut Request<Body>) -> Option<Response> {
    if matches!(req.method(), &Method::GET | &Method::HEAD) {
        let path = req.uri().path().to_string();
        // Phase 18: static files live only under the app's four asset roots
        // plus two exact root files; everything else (status.php, OCS, DAV,
        // index.php, …) skips the fs stat entirely.  This also stops serving
        // repo files (AUTHORS, 3rdparty/*, dotfiles) that real nginx
        // installs deny.  `/index.html` preserves the install page; GET /
        // itself still falls through (the root is a directory).
        const STATIC_PREFIXES: [&str; 4] = ["/core/", "/dist/", "/themes/", "/apps/"];
        let is_static = STATIC_PREFIXES.iter().any(|p| path.starts_with(p))
            || matches!(path.as_str(), "/robots.txt" | "/index.html");
        // Skip PHP scripts and any path that looks like traversal.
        if is_static && !path.contains(".php") && !path.contains("..") {
            let candidate = state.nc_root.join(path.trim_start_matches('/'));
            if tokio::fs::metadata(&candidate)
                .await
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                // Serve path consumes the request; the composite is done.
                let owned = std::mem::take(req);
                return Some(match ServeDir::new(&state.nc_root).oneshot(owned).await {
                    Ok(resp) => resp.into_response(),
                    Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                });
            }
        }
    }
    None
}

/// Composite request middleware (Phase 18.6): static files → maintenance
/// guard → auth, inside ONE `from_fn` layer instead of three.  Each check
/// preserves the exact early-return semantics and response bytes of the
/// former middleware; the composite adds only the Set-Cookie forwarding from
/// the PHP session resolver (remember-me rotation), which ran after the
/// handler in the original auth middleware and does the same here.
pub(crate) async fn http_middleware_stack(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    // Phase 15 F2: resolve the client identity once per request — ported
    // from PHP's Request (getRemoteAddress/getServerProtocol/
    // getInsecureServerHost) — and enforce trusted domains (base.php:872-912).
    let identity = crate::client_identity::resolve(&req.headers(), peer.ip(), &state.nc_config);

    // 1. Static files (outermost — bypasses auth, enforcement, maintenance;
    //    JS/CSS/images are public, exactly like the webserver serving them
    //    before PHP boots).
    if let Some(resp) = try_static_files_check(&state, &mut req).await {
        return resp;
    }
    // 1b. Trusted-domain enforcement (PHP runs it inside OC::init for every
    //     booted request).  The css exemption mirrors PATH_INFO semantics:
    //     a leading /index.php script segment is stripped first.
    {
        let path = req.uri().path();
        let path_info = path.strip_prefix("/index.php").unwrap_or(path);
        let uri_with_query = match req.uri().query() {
            Some(q) => format!("{path}?{q}"),
            None => path.to_string(),
        };
        if let Some(resp) = crate::client_identity::trusted_domains_response(
            &state.nc_config,
            peer.ip(),
            &req.headers(),
            &uri_with_query,
            path_info,
        ) {
            return resp;
        }
    }
    // 2. Maintenance guard (503 except /status.php, /heartbeat).
    if let Some(resp) = maintenance_check(&state, req.uri().path()).await {
        return resp;
    }
    // The identity is consumed by auth (throttle key, SameSite is_https) and
    // the FastCGI proxy (REMOTE_ADDR / SERVER_NAME / SERVER_PORT / HTTPS).
    req.extensions_mut().insert(identity);
    // 3. Auth (bearer/basic/session) — attaches `AuthInfo` or rejects.
    let set_cookies = match auth_check(&state, &mut req).await {
        Ok(cookies) => cookies,
        Err(resp) => return resp,
    };
    let mut resp = next.run(req).await;
    for cookie_val in &set_cookies {
        if let Ok(hv) = axum::http::HeaderValue::from_str(cookie_val) {
            resp.headers_mut()
                .append(axum::http::header::SET_COOKIE, hv);
        }
    }
    resp
}

/// Proxied DAV sub-trees (Phase 18) — served by PHP/SabreDAV, not the native
/// files handler.  Mirrors the explicit route registrations they replaced
/// (10 subtrees × 2 mount prefixes).  `/uploads` is native and deliberately
/// NOT in this list.
const PROXIED_DAV_SUBTREES: [&str; 10] = [
    "/versions",
    "/comments",
    "/trashbin",
    "/principals",
    "/calendars",
    "/public-calendars",
    "/system-calendars",
    "/addressbooks",
    "/avatars",
    "/access-control",
];

/// DAV arbiter handler — the single classified entry for both mount roots
/// (`/remote.php/dav`, `/dav`).
///
/// Phase 18: replaced the ~30 explicit mount routes with one wildcard pair
/// per root; classification happens here:
///   SEARCH/REPORT          → PHP-FPM (DASL search, sync-collection)
///   proxied subtree prefix → PHP-FPM (versions, comments, trashbin, …)
///   /uploads               → native upload handler (Phase 5.5)
///   /bulk (POST)           → native bulk handler (Phase 5.9)
///   everything else        → native files tree (dav_handler)
async fn dav_arbiter_handler(State(state): State<AppState>, req: Request<Body>) -> Response {
    // Path remainder after the mount root.  Trim in this order so a
    // "/dav/…" path cannot consume the "/remote.php/dav" prefix.
    let remainder = req
        .uri()
        .path()
        .trim_start_matches("/remote.php/dav")
        .trim_start_matches("/dav");
    let method = req.method().as_str();

    // Proxied subtrees take precedence over the SEARCH/REPORT rule so a
    // SEARCH against e.g. /remote.php/dav/versions behaves as it did with
    // the explicit any()-method proxy routes.
    if PROXIED_DAV_SUBTREES
        .iter()
        .any(|p| remainder.starts_with(p))
        || matches!(method, "SEARCH" | "REPORT")
    {
        if let Some(ref fpm) = state.fastcgi {
            return nc_fastcgi::proxy_handler(fpm, req).await;
        }
        // No PHP-FPM configured → fall through; the native handler
        // will return 405 / 501.
        return nc_dav::dav_handler(State(nc_dav::NcDavState::from_ref(&state)), req).await;
    }

    if remainder.starts_with("/uploads") {
        return nc_dav::upload_handler(State(nc_dav::NcDavState::from_ref(&state)), req).await;
    }
    // Non-POST /bulk falls through to the files tree (404) — sabreDAV treats
    // "bulk" as an ordinary resource path, so this is PHP-faithful (the old
    // post-only route returned an axum 405).
    if remainder == "/bulk" && method == "POST" {
        return nc_dav::bulk_handler(State(nc_dav::NcDavState::from_ref(&state)), req).await;
    }

    nc_dav::dav_handler(State(nc_dav::NcDavState::from_ref(&state)), req).await
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
        // Phase 11.2: native preview/thumbnail hit-serving.  Cache hits for the
        // caller's own home-storage files are served with zero PHP; misses, shared
        // files, and unauthenticated requests proxy to PHP-FPM (the handlers decide).
        // More specific than the registry's `/apps/files/{*tail}` / `/core/{*tail}`
        // wildcards, so axum prefers these.
        .route("/core/preview", get(crate::preview::preview_by_file_id))
        .route(
            "/index.php/core/preview",
            get(crate::preview::preview_by_file_id),
        )
        .route("/core/preview.png", get(crate::preview::preview_by_path))
        .route(
            "/index.php/core/preview.png",
            get(crate::preview::preview_by_path),
        )
        .route(
            "/apps/files/api/v1/thumbnail/{x}/{y}/{*file}",
            get(crate::preview::files_thumbnail),
        )
        .route(
            "/index.php/apps/files/api/v1/thumbnail/{x}/{y}/{*file}",
            get(crate::preview::files_thumbnail),
        )
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
        // DAV (Phase 4) — both mount roots (`/remote.php/dav`, `/dav`) are
        // served by the single classified arbiter handler (Phase 18): the
        // proxied subtrees, uploads, bulk, and the native files tree are
        // dispatched inside it, so the route table stays tiny (6 routes
        // instead of ~30 — axum clones the whole router per request, so
        // every route costs CPU on every request).
        // Each root needs three routes because axum's `{*path}` catch-all
        // requires at least one character — it does NOT match a bare trailing
        // slash.  WebDAV clients routinely PROPFIND the collection root with a
        // trailing slash, so we register:
        //   /mount          — exact, no trailing slash
        //   /mount/         — exact, trailing slash only (collection root)
        //   /mount/{*path}  — one or more path segments
        .route("/remote.php/dav", axum::routing::any(dav_arbiter_handler))
        .route("/remote.php/dav/", axum::routing::any(dav_arbiter_handler))
        .route(
            "/remote.php/dav/{*path}",
            axum::routing::any(dav_arbiter_handler),
        )
        .route("/dav", axum::routing::any(dav_arbiter_handler))
        .route("/dav/", axum::routing::any(dav_arbiter_handler))
        .route("/dav/{*path}", axum::routing::any(dav_arbiter_handler))
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
    //   trace_layer       — log every request: method, path, status, duration
    //   http_middleware_stack — static files → maintenance → auth in one
    //                       from_fn layer (Phase 18.6)
    //   routes            — native handlers + PHP-FPM proxy
    let trace_layer = TraceLayer::new_for_http()
        .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
        .on_response(trace::DefaultOnResponse::new().level(Level::INFO));

    // Phase 18.6: the three request middlewares (static → maintenance →
    // auth) run inside ONE from_fn layer — one wrapper chain instead of
    // three, ~2 fewer wrapper polls per await per request.
    r.layer(middleware::from_fn_with_state(
        state.clone(),
        http_middleware_stack,
    ))
    .layer(trace_layer)
    .with_state(state)
}
