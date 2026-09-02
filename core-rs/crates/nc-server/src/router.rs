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
use nc_db::config::NcConfig;

/// Derive the static-asset whitelist prefixes from config — PHP's
/// `OC::$APPSROOTS` (`lib/base.php:157-175`) mapped to web paths.
///
/// The server's own three asset roots (`/core/ /dist/ /themes/`) are fixed;
/// app-level assets use every `apps_paths` entry's `url`, rtrimmed of
/// trailing `/` exactly like PHP, with PHP's default-root fallback
/// (`/apps`, only when that directory exists — `base.php:167-170`).
/// Absent/empty `apps_paths` therefore yields the same surface as the
/// original hardcoded whitelist, while custom app dirs (e.g. `/wapps` for
/// memories on the live install) become servable in standalone mode.
///
/// Deliberate guards (see phase-18.md Changes):
/// - A `url` that is empty or `/` after rtrim is skipped — as a prefix it
///   would make every request path a static candidate, resurrecting the
///   per-request fs stat Phase 18.1 removed (PHP tolerates webroot-root
///   apps; the canonical deployment serves them via nginx's extension
///   regex instead, which costs no fs stat).
/// - A `url` not starting with `/` is skipped (a browser-relative web path —
///   broken config in PHP too, `getAppWebPath` would emit `WEBROOT + url`).
/// - Duplicate urls are collapsed (PHP de-duplicates nothing; the whitelist
///   only needs the set).
pub(crate) fn static_prefixes_from_config(cfg: &NcConfig, nc_root: &std::path::Path) -> Vec<String> {
    let mut prefixes = vec![
        "/core/".to_string(),
        "/dist/".to_string(),
        "/themes/".to_string(),
    ];
    let mut seen: std::collections::HashSet<String> = prefixes.iter().cloned().collect();

    let app_urls: Vec<&str> = match &cfg.apps_paths {
        // PHP: a non-empty `apps_paths` replaces the default entirely;
        // entries lacking `url`/`path` are skipped (`isset` checks).
        Some(paths) if !paths.is_empty() => {
            paths.iter().filter_map(|p| p.url.as_deref()).collect()
        }
        _ => {
            if nc_root.join("apps").is_dir() {
                vec!["/apps"]
            } else {
                tracing::warn!(
                    "no apps_paths configured and {}/apps missing — app static assets will not be served",
                    nc_root.display()
                );
                vec![]
            }
        }
    };
    for url in app_urls {
        // PHP rtrims both keys (`base.php:161-162`).
        let url = url.trim_end_matches('/');
        if url.is_empty() || url == "/" {
            tracing::warn!(
                "apps_paths url {url:?} skipped: a webroot-root path would make every request a static candidate"
            );
            continue;
        }
        if !url.starts_with('/') {
            tracing::warn!("apps_paths url {url:?} skipped: not a web-root-relative path");
            continue;
        }
        let prefix = format!("{url}/");
        if seen.insert(prefix.clone()) {
            prefixes.push(prefix);
        }
    }
    prefixes
}

/// Path prefixes where the edge SameSite gate applies — a subset of the
/// Rust-native surface (every route whose handler is NOT `php_fpm_fallback`).
///
/// PHP enforces the strict-cookie check in two layers:
/// 1. `base.php` `performSameSiteCookieProtection` (base.php:560-611,
///    invoked from `OC::init` at base.php:773) — for every script except
///    `index.php`, `cron.php`, `public.php` (base.php:588-591).
/// 2. The AppFramework `SecurityMiddleware` (SecurityMiddleware.php:190-195)
///    — for index.php routes, annotation-driven: skipped when the route is
///    `@NoCSRFRequired`.
///
/// Rust replicates layer 1 at the edge for the scripts it serves natively:
/// remote.php (DAV), the OCS scripts, and the native index.php preview /
/// thumbnail routes (PHP's middleware gates those too — none of the native
/// handlers is `@NoCSRFRequired`).  The native Photos preview route is the
/// deliberate exception: `Photos\PreviewController::index` is
/// `@NoCSRFRequired`, so the edge must not gate it either.  Requests proxied
/// to PHP-FPM are exempt on purpose — PHP's own pipeline decides there, with
/// annotation knowledge the edge cannot have.  Gating a proxied index.php
/// route 412s cross-site flows PHP passes: the OIDC login callback
/// `/index.php/apps/user_oidc/code` is `#[NoCSRFRequired]` (the `state` param
/// is the CSRF protection), and browsers withhold the SameSite=Strict guard
/// cookie on the cross-site redirect from the identity provider — an edge gate
/// would break every Safari OIDC login.
///
/// Must be kept in sync with the native route registrations in [`build`]:
/// /remote.php* (webdav, dav arbiter incl. uploads), /dav*, /ocs/v1.php,
/// /ocs/v2.php (native OCS routes and the proxy catch-all — PHP gates the
/// v1.php/v2.php scripts at base.php either way), and the native preview /
/// thumbnail routes — except `/apps/photos/api/v1/preview/{fileId}`, which is
/// native but ungated because the Photos controller is `@NoCSRFRequired`.
/// `/status.php` and `/heartbeat` are absent because `auth_check` returns
/// before the gate for them.
pub(crate) fn samesite_gated_prefixes() -> Vec<String> {
    vec![
        "/remote.php".to_string(),
        "/dav".to_string(),
        "/ocs/v1.php".to_string(),
        "/ocs/v2.php".to_string(),
        "/core/preview".to_string(),
        "/index.php/core/preview".to_string(),
        "/apps/files/api/v1/thumbnail/".to_string(),
        "/index.php/apps/files/api/v1/thumbnail/".to_string(),
    ]
}

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
/// The candidate prefixes are config-derived (see
/// [`static_prefixes_from_config`]): `/core/`, `/dist/`, `/themes/`, every
/// `apps_paths` `url` (e.g. `/apps/`, `/wapps/`), plus the two exact root
/// files.  Everything else (status.php, OCS, DAV, index.php, …) skips the fs
/// stat entirely — this also stops serving repo files (AUTHORS, 3rdparty/*,
/// dotfiles) that real nginx installs deny.  `/index.html` preserves the
/// install page; GET / itself still falls through (the root is a directory).
async fn try_static_files_check(state: &AppState, req: &mut Request<Body>) -> Option<Response> {
    if matches!(req.method(), &Method::GET | &Method::HEAD) {
        let path = req.uri().path().to_string();
        let is_static = state
            .static_prefixes
            .iter()
            .any(|p| path.starts_with(p))
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

/// DAV classification — `true` when Rust serves the path itself (no real
/// PHP request follows), `false` when it is proxied to PHP-FPM.
///
/// Single source of truth for BOTH the arbiter dispatch below and the auth
/// middleware's session-resolve gating (middleware/auth.rs): a session
/// resolve for a proxied path must stay read-only, because the real request
/// runs `OC::handleLogin()` itself.
///
///   SEARCH/REPORT          → PHP-FPM (DASL search, sync-collection)
///   proxied subtree prefix → PHP-FPM (versions, comments, trashbin, …)
///   /uploads               → native upload handler (Phase 5.5)
///   /bulk (POST)           → native bulk handler (Phase 5.9)
///   /files                 → native files tree (dav_handler)
///   everything else        → PHP-FPM.  PHP's sabre root is dynamic — fixed
///                             children plus app-registered collections
///                             (photos, photospublic, …), so a static list
///                             can never be complete; PHP is the registrar
///                             of its own root (phase-18.md Changes
///                             2026-08-16).
pub(crate) fn dav_served_by_rust(path: &str, method: &str) -> bool {
    // Path remainder after the mount root.  Trim in this order so a
    // "/dav/…" path cannot consume the "/remote.php/dav" prefix.
    let remainder = path
        .trim_start_matches("/remote.php/dav")
        .trim_start_matches("/dav");

    // Proxied subtrees take precedence over the SEARCH/REPORT rule so a
    // SEARCH against e.g. /remote.php/dav/versions behaves as it did with
    // the explicit any()-method proxy routes.
    if PROXIED_DAV_SUBTREES
        .iter()
        .any(|p| remainder.starts_with(p))
        || matches!(method, "SEARCH" | "REPORT")
    {
        return false;
    }

    if remainder.starts_with("/uploads") {
        return true;
    }
    // Non-POST /bulk falls through to the files tree (404) — sabreDAV treats
    // "bulk" as an ordinary resource path, so this is PHP-faithful (the old
    // post-only route returned an axum 405).
    if remainder == "/bulk" && method == "POST" {
        return true;
    }

    // ── The native files tree — the hot path ───────────────────────────────
    // Classified explicitly now that the default branch proxies to PHP:
    // everything else (the dav root itself, app-registered collections such
    // as photos/systemtags/provisioning, any future subtree) belongs to
    // PHP's sabre tree.  PHP is the registrar of its own root collection
    // (apps/dav/lib/Server.php:415-417 mounts app collections;
    // RootCollection.php:189-217 the fixed ones) — a static proxy list can
    // never enumerate them, and the native files tree 404s anything that is
    // not a filecache path (the live Photos Places PROPFIND
    // /photos/{uid}/places/ died this way with an empty body).
    remainder.starts_with("/files/") || remainder == "/files"
}

/// DAV arbiter handler — the single classified entry for both mount roots
/// (`/remote.php/dav`, `/dav`).
///
/// Phase 18: replaced the ~30 explicit mount routes with one wildcard pair
/// per root; the classification itself lives in [`dav_served_by_rust`].
async fn dav_arbiter_handler(State(state): State<AppState>, req: Request<Body>) -> Response {
    let path = req.uri().path();
    let method = req.method().as_str();

    // Everything else → PHP-FPM; PHP decides whether the path exists.
    if !dav_served_by_rust(path, method) {
        if let Some(ref fpm) = state.fastcgi {
            return nc_fastcgi::proxy_handler(fpm, req).await;
        }
        // No PHP-FPM configured → fall through; the native handler
        // will return 405 / 501.
        return nc_dav::dav_handler(State(nc_dav::NcDavState::from_ref(&state)), req).await;
    }

    // ── Native handlers ─────────────────────────────────────────────────────
    // /uploads → upload handler; /bulk (POST) → bulk handler; /files → files tree.
    let remainder = path
        .trim_start_matches("/remote.php/dav")
        .trim_start_matches("/dav");

    if remainder.starts_with("/uploads") {
        return nc_dav::upload_handler(State(nc_dav::NcDavState::from_ref(&state)), req).await;
    }
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
        .route(
            "/apps/photos/api/v1/preview/{file_id}",
            get(crate::preview::photos_preview),
        )
        .route(
            "/index.php/apps/photos/api/v1/preview/{file_id}",
            get(crate::preview::photos_preview),
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
        // Direct-download tokens (Phase 7.6): `POST /ocs/v2.php/.../direct`
        // mints a token, then the client streams `GET /remote.php/direct/{token}`.
        // Rust serves the hot DAV files tree natively; this cold token path is
        // delegated to PHP-FPM, which resolves `oc_directlink` through
        // DirectHome/DirectFile (incl. the view-only event) and streams the file.
        .route("/remote.php/direct", axum::routing::any(php_fpm_fallback))
        .route(
            "/remote.php/direct/{*path}",
            axum::routing::any(php_fpm_fallback),
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The DAV classification is load-bearing for BOTH the arbiter dispatch
    /// and the auth middleware's session-resolve login gating — a regression
    /// here silently changes which requests get a PHP-side login in the
    /// resolve (and which proxy instead of being served natively).
    #[test]
    fn dav_served_by_rust_files_tree_is_native() {
        assert!(dav_served_by_rust("/dav/files", "GET"));
        assert!(dav_served_by_rust("/dav/files/", "PROPFIND"));
        assert!(dav_served_by_rust("/dav/files/admin/test.txt", "GET"));
        assert!(dav_served_by_rust("/remote.php/dav/files/admin/", "PROPFIND"));
        assert!(dav_served_by_rust("/remote.php/dav/files", "GET"));
    }

    #[test]
    fn dav_served_by_rust_uploads_and_bulk_are_native() {
        assert!(dav_served_by_rust("/dav/uploads/admin", "PUT"));
        assert!(dav_served_by_rust("/dav/uploads/admin/chunk-1", "PUT"));
        assert!(dav_served_by_rust("/dav/bulk", "POST"));
        // Non-POST /bulk is an ordinary resource path → PHP-faithful 404.
        assert!(!dav_served_by_rust("/dav/bulk", "GET"));
    }

    #[test]
    fn dav_served_by_rust_proxied_cases() {
        // Proxied subtrees (versions, comments, trashbin, …) → PHP.
        assert!(!dav_served_by_rust("/dav/versions/admin", "GET"));
        assert!(!dav_served_by_rust("/remote.php/dav/trashbin/admin", "GET"));
        assert!(!dav_served_by_rust("/dav/comments/1", "PROPFIND"));
        // SEARCH/REPORT always → PHP (DASL search, sync-collection).
        assert!(!dav_served_by_rust("/dav/files/admin", "SEARCH"));
        assert!(!dav_served_by_rust("/dav/files/admin", "REPORT"));
        // DAV root + app-registered collections → PHP (sabre registrar).
        assert!(!dav_served_by_rust("/dav", "PROPFIND"));
        assert!(!dav_served_by_rust("/dav/", "PROPFIND"));
        assert!(!dav_served_by_rust("/dav/photos/admin", "PROPFIND"));
        assert!(!dav_served_by_rust("/dav/systemtags", "PROPFIND"));
        // Non-files remote.php paths (webdav alias etc.) → PHP.
        assert!(!dav_served_by_rust("/remote.php/webdav/test.txt", "GET"));
        assert!(!dav_served_by_rust("/index.php/apps/files", "GET"));
    }

    /// `/remote.php/direct` is a cold, PHP-owned token path — Rust must not
    /// claim it natively.  The router registers it as `php_fpm_fallback`
    /// (see `build`), so this guard only needs to assert the classification
    /// stays out of the native DAV tree; the fallback route itself is covered
    /// by the exact route registration below.
    #[test]
    fn dav_served_by_rust_direct_is_not_native() {
        assert!(!dav_served_by_rust("/remote.php/direct/token123", "GET"));
        assert!(!dav_served_by_rust("/remote.php/direct/", "GET"));
    }

    /// Build a config whose `apps_paths` contains one entry per url.
    fn cfg_with_apps_paths(urls: &[&str]) -> NcConfig {
        let entries: Vec<serde_json::Value> = urls
            .iter()
            .map(|u| {
                serde_json::json!({
                    "path": format!("/var/www/html/{}", u.trim_start_matches('/')),
                    "url": u,
                    "writable": true,
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({ "apps_paths": entries })).expect("config parse")
    }

    /// Temp webroot with an `apps/` dir (PHP's default-root fallback needs
    /// the directory to exist); unique per test+process.
    fn root_with_apps_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nc-static-prefixes-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("apps")).expect("mkdir apps");
        dir
    }

    #[test]
    fn default_config_uses_apps_fallback() {
        // No `apps_paths` + an existing `apps/` dir → PHP's default root.
        let cfg: NcConfig = serde_json::from_str("{}").expect("empty config");
        let root = root_with_apps_dir();
        let prefixes = static_prefixes_from_config(&cfg, &root);
        assert_eq!(
            prefixes,
            vec![
                "/core/".to_string(),
                "/dist/".to_string(),
                "/themes/".to_string(),
                "/apps/".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_apps_dir_drops_the_fallback() {
        let cfg: NcConfig = serde_json::from_str("{}").expect("empty config");
        let root = std::env::temp_dir().join(format!("nc-no-apps-{}", std::process::id()));
        let prefixes = static_prefixes_from_config(&cfg, &root);
        assert_eq!(
            prefixes,
            vec!["/core/".to_string(), "/dist/".to_string(), "/themes/".to_string()]
        );
    }

    #[test]
    fn apps_paths_replace_the_default() {
        // A non-empty `apps_paths` replaces the `/apps` fallback entirely.
        let cfg = cfg_with_apps_paths(&["/wapps", "/custom_apps"]);
        let root = root_with_apps_dir();
        let prefixes = static_prefixes_from_config(&cfg, &root);
        assert_eq!(
            prefixes,
            vec![
                "/core/".to_string(),
                "/dist/".to_string(),
                "/themes/".to_string(),
                "/wapps/".to_string(),
                "/custom_apps/".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trailing_slash_is_trimmed() {
        // PHP rtrims `url` (`base.php:161`); both spellings yield one prefix.
        let cfg = cfg_with_apps_paths(&["/wapps/", "/apps"]);
        let root = root_with_apps_dir();
        let prefixes = static_prefixes_from_config(&cfg, &root);
        assert_eq!(
            prefixes,
            vec![
                "/core/".to_string(),
                "/dist/".to_string(),
                "/themes/".to_string(),
                "/wapps/".to_string(),
                "/apps/".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn webroot_root_and_bare_urls_are_skipped() {
        // `/` or `` as a prefix would make every request a static candidate
        // (the per-request fs stat Phase 18.1 removed); a non-slash url is a
        // browser-relative web path, broken in PHP too.
        let cfg = cfg_with_apps_paths(&["/", "", "apps"]);
        let root = root_with_apps_dir();
        let prefixes = static_prefixes_from_config(&cfg, &root);
        assert_eq!(
            prefixes,
            vec!["/core/".to_string(), "/dist/".to_string(), "/themes/".to_string()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_urls_collapse() {
        let cfg = cfg_with_apps_paths(&["/apps", "/apps"]);
        let root = root_with_apps_dir();
        let prefixes = static_prefixes_from_config(&cfg, &root);
        assert_eq!(prefixes.iter().filter(|p| *p == "/apps/").count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The edge SameSite gate's scope must cover exactly the gated native
    /// surface: remote.php (webdav + dav arbiter + uploads), the /dav alias,
    /// the OCS scripts (native routes AND the proxy catch-all — PHP gates the
    /// v1.php/v2.php scripts at base.php either way), and the native preview /
    /// thumbnail routes except the Photos API (its controller is
    /// `@NoCSRFRequired`).  Proxied index.php / app / public.php / login /
    /// root / well-known paths are deliberately absent (PHP's annotation-aware
    /// middleware decides there).
    #[test]
    fn samesite_gated_prefixes_cover_the_native_surface() {
        let prefixes = samesite_gated_prefixes();
        for native in [
            "/remote.php/webdav/",
            "/remote.php/dav/files/alice/",
            "/remote.php/dav/uploads/",
            "/dav/files/alice/",
            "/ocs/v1.php/config",
            "/ocs/v2.php/cloud/capabilities",
            "/core/preview",
            "/core/preview.png",
            "/index.php/core/preview",
            "/apps/files/api/v1/thumbnail/32/32/test.png",
            "/index.php/apps/files/api/v1/thumbnail/32/32/test.png",
        ] {
            assert!(
                prefixes.iter().any(|p| native.starts_with(p)),
                "{native} must be SameSite-gated"
            );
        }
        // Native-but-ungated (Photos `@NoCSRFRequired`) and proxied routes both
        // defer the edge gate to PHP's annotation-aware middleware.
        for ungated in [
            "/index.php/apps/user_oidc/code",
            "/index.php/apps/photos/api/v1/preview/42",
            "/apps/photos/api/v1/preview/42",
            "/index.php/login",
            "/index.php",
            "/public.php/webdav",
            "/login",
            "/.well-known/webfinger",
            "/apps/files/",
            "/",
            "/ocs-provider/index.php",
        ] {
            assert!(
                !prefixes.iter().any(|p| ungated.starts_with(p)),
                "{ungated} must defer the SameSite gate to PHP"
            );
        }
    }

    #[test]
    fn entries_without_url_are_skipped() {
        // PHP skips entries lacking `url` or `path` (`isset` checks); a
        // path-only entry contributes no web path.
        let cfg: NcConfig = serde_json::from_value(serde_json::json!({
            "apps_paths": [
                {"path": "/var/www/html/wapps", "url": "/wapps"},
                {"path": "/var/www/html/other"}
            ]
        }))
        .expect("config parse");
        let root = root_with_apps_dir();
        let prefixes = static_prefixes_from_config(&cfg, &root);
        assert_eq!(
            prefixes,
            vec![
                "/core/".to_string(),
                "/dist/".to_string(),
                "/themes/".to_string(),
                "/wapps/".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
