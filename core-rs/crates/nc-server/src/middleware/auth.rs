use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use nc_auth::{AuthInfo, AuthMethod};

use crate::state::AppState;

// ── Public paths that do NOT require any auth (even for error responses) ──────
// These routes are served with anonymous access; the middleware still tries
// to extract credentials but will not return 401 if they are absent.
#[allow(dead_code)] // used when route-level exemption logic is wired in
fn is_public_path(path: &str) -> bool {
    matches!(
        path,
        "/status.php" | "/heartbeat" | "/ocs/v1.php/config" | "/ocs/v2.php/config"
    ) || path == "/ocs/v1.php/cloud/capabilities"
        || path == "/ocs/v2.php/cloud/capabilities"
}

/// Returns `true` for paths that use DAV-style auth (realm = "Nextcloud").
///
/// REQ §4.3: DAV endpoints respond with `Basic realm="Nextcloud"`.
/// REQ §5.4: OCS endpoints respond with `Basic realm="Authorisation Required"`.
fn is_dav_path(path: &str) -> bool {
    path.starts_with("/remote.php") || path.starts_with("/dav") || path.starts_with("/public.php")
}

/// Auth middleware (Phase 3).
///
/// Executed AFTER the maintenance guard in axum's layer stack.
///
/// Behaviour per request:
/// - Extract credentials from `Authorization` header (Bearer → Basic priority).
/// - If valid credentials: attach `AuthInfo` extension; update last_activity.
/// - If invalid credentials: return `401` immediately (brute-force recorded).
/// - If no credentials: anonymous — no extension attached; handler decides
///   whether to require auth.
pub async fn auth_layer(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let _method = req.method().as_str().to_string();

    // Skip auth processing for /status.php and /heartbeat entirely.
    if matches!(path.as_str(), "/status.php" | "/heartbeat") {
        return next.run(req).await;
    }

    // ── Header extraction ─────────────────────────────────────────────────
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let ocs_api_request = req
        .headers()
        .get("ocs-apirequest")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let user_agent = req
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // REQ §4.3 / §5.4: XHR detection via `X-Requested-With` header.
    // DummyBasic is sent for XHR to prevent the browser's native auth dialog.
    let is_xhr = req
        .headers()
        .get("x-requested-with")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("XMLHttpRequest"))
        .unwrap_or(false);

    let is_dav = is_dav_path(&path);
    let client_ip = extract_client_ip(&req);

    let app_secret = state
        .appconfig_cache
        .read()
        .expect("appconfig lock")
        .get_string("core", "secret")
        .unwrap_or_default();

    // ── CSRF check ────────────────────────────────────────────────────────
    // For Phase 3, we only enforce CSRF if session cookies are present AND
    // the strict cookie guard fails (task 3.5).
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let is_https = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|p| p == "https")
        .unwrap_or(false);

    use nc_auth::session::{check_samesite_cookies, CookieCheck};
    if auth_header.is_none() && !ocs_api_request {
        match check_samesite_cookies(&cookie_header, &state.instanceid, is_https) {
            CookieCheck::StrictCheckFailed => {
                // PHP returns 412 Precondition Failed for strict cookie check
                // failures (base.php strict cookie check, REQ §7.9.2).
                return (
                    StatusCode::PRECONDITION_FAILED,
                    "SameSite cookie check failed",
                )
                    .into_response();
            }
            _ => {}
        }
    }

    // ── Credential extraction and verification ────────────────────────────
    // `pending_set_cookies` collects any `Set-Cookie` header values returned
    // by the PHP-FPM `__session_resolve` endpoint (remember-me token rotation
    // path — §7.9.3/§7.9.4).  They are appended to the final HTTP response
    // after the downstream handler completes.
    let mut pending_set_cookies: Vec<String> = Vec::new();
    let auth_info: Option<AuthInfo> = match &auth_header {
        Some(ah) if ah.starts_with("Bearer ") => {
            // REQ §4.6: throttle bearer attempts the same as Basic (brute-force
            // protection applies to all credential types).
            let throttle = nc_auth::bruteforce::check_throttle(
                "login",
                &client_ip,
                state.nc_config.bruteforce_protection_enabled,
                &state.appconfig_cache,
                &state.pool,
                &state.table_prefix,
            )
            .await;

            if throttle.should_reject {
                return build_429(throttle.retry_after_secs);
            }
            if let Some(delay) = throttle.delay {
                tokio::time::sleep(delay).await;
            }

            match nc_auth::bearer::lookup_bearer(
                nc_auth::bearer::extract_bearer(ah).unwrap_or(""),
                &state.pool,
                &state.token_cache,
                &app_secret,
                &state.table_prefix,
            )
            .await
            {
                Some(cached) => {
                    // Fire-and-forget last_activity update (task 3.4).
                    nc_auth::bearer::spawn_last_activity_update(
                        cached.id,
                        state.pool.clone(),
                        state.table_prefix.clone(),
                    );

                    // 2FA gate (task 3.6) — REQ §4.5.
                    if nc_auth::twofa::requires_2fa(
                        &cached.uid,
                        cached.token_type,
                        &state.pool,
                        &state.table_prefix,
                    )
                    .await
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            "Not Authenticated: 2FA challenge not passed.",
                        )
                            .into_response();
                    }

                    // Phase 7.2: admin group check.
                    let is_admin =
                        nc_auth::is_admin_user(&cached.uid, &state.pool, &state.table_prefix).await;

                    // Extract the raw bearer value for HTTP_X_NC_SESSION_TOKEN.
                    let raw_token = nc_auth::bearer::extract_bearer(ah).map(str::to_owned);

                    Some(AuthInfo {
                        uid: cached.uid.clone(),
                        is_admin,
                        method: AuthMethod::Bearer,
                        token_id: Some(cached.id),
                        raw_token,
                    })
                }
                None => {
                    // Invalid bearer token — record and reject.
                    nc_auth::bruteforce::record_attempt(
                        "login",
                        &client_ip,
                        &state.pool,
                        &state.table_prefix,
                    )
                    .await;
                    // Bearer failures: no WWW-Authenticate by default (REQ §4.3).
                    // Exception: if oauth2 is enabled and the client is mirall,
                    // send `Bearer realm="Nextcloud"` so the desktop client can
                    // trigger an OAuth2 flow.
                    let send_www_auth = state
                        .appconfig_cache
                        .read()
                        .expect("appconfig lock")
                        .get_bool("oauth2", "enable_oc_clients")
                        && user_agent.contains("mirall");
                    return build_401(send_www_auth, is_xhr, is_dav, false);
                }
            }
        }
        Some(ah) if ah.starts_with("Basic ") => {
            // Check brute-force throttle before verifying credentials (REQ §4.6).
            let throttle = nc_auth::bruteforce::check_throttle(
                "login",
                &client_ip,
                state.nc_config.bruteforce_protection_enabled,
                &state.appconfig_cache,
                &state.pool,
                &state.table_prefix,
            )
            .await;

            if throttle.should_reject {
                return build_429(throttle.retry_after_secs);
            }
            if let Some(delay) = throttle.delay {
                tokio::time::sleep(delay).await;
            }

            match nc_auth::basic::extract_basic(ah) {
                None => {
                    return build_401(true, is_xhr, is_dav, false);
                }
                Some((login, password)) => {
                    // REQ §4.1/4.2: try app token first, then plain password.
                    match nc_auth::basic::verify_basic(
                        &login,
                        &password,
                        &state.pool,
                        &state.table_prefix,
                        &app_secret,
                    )
                    .await
                    {
                        Some(result) => {
                            // 2FA gate for Basic auth (REQ §4.5).
                            let token_type = result.token_type.unwrap_or(0);
                            if nc_auth::twofa::requires_2fa(
                                &result.uid,
                                token_type,
                                &state.pool,
                                &state.table_prefix,
                            )
                            .await
                            {
                                return (
                                    StatusCode::UNAUTHORIZED,
                                    "Not Authenticated: 2FA challenge not passed.",
                                )
                                    .into_response();
                            }
                            // Phase 7.2: admin group check.
                            let is_admin = nc_auth::is_admin_user(
                                &result.uid,
                                &state.pool,
                                &state.table_prefix,
                            )
                            .await;
                            // For Basic auth with an app token, store the raw
                            // password (the token value) for HTTP_X_NC_SESSION_TOKEN.
                            // For plain-password auth, do not forward the password.
                            let raw_token = if result.token_id.is_some() {
                                nc_auth::basic::extract_basic(ah).map(|(_, pwd)| pwd)
                            } else {
                                None
                            };
                            Some(AuthInfo {
                                uid: result.uid,
                                is_admin,
                                method: AuthMethod::Basic,
                                token_id: result.token_id,
                                raw_token,
                            })
                        }
                        None => {
                            nc_auth::bruteforce::record_attempt(
                                "login",
                                &client_ip,
                                &state.pool,
                                &state.table_prefix,
                            )
                            .await;
                            return build_401(true, is_xhr, is_dav, false);
                        }
                    }
                }
            }
        }
        _ => {
            // No Authorization header — try PHP session cookie resolution
            // (§7.9.6).
            //
            // Browser requests authenticate via the PHP login flow and carry
            // only cookies on subsequent requests; the Authorization header is
            // absent.  We ask the PHP-FPM shim's `__session_resolve` endpoint
            // to run `OC::handleLogin()` and return the resolved identity.
            //
            // Guard: PHP-FPM must be configured — without it there is no
            // `__session_resolve` endpoint and no session cache to populate.
            let (Some(fpm), Some(session_cache)) = (&state.fastcgi, &state.session_cache) else {
                // PHP-FPM not configured — treat as anonymous.
                return next.run(req).await;
            };

            // Find the raw session-cookie value (PHP session cookie keyed on
            // `{instanceid}`, or `nc_token` for the remember-me fallback).
            // PHP's `cookieCheckRequired()` uses the same two cookies as the
            // session trigger (Request.php:464-470).
            let Some(raw_val) =
                nc_auth::session::session_cookie_value(&state.instanceid, &cookie_header)
            else {
                // No session cookies at all — anonymous request.
                return next.run(req).await;
            };
            let raw_val = raw_val.to_owned();

            // ── Session identity cache lookup (§7.9.5) ──────────────────────
            // Key: SHA-256(raw PHP session cookie value).
            let cache_key = nc_auth::make_cache_key(&raw_val);
            let identity = if let Some(cached) = nc_auth::cache_lookup(session_cache, &cache_key) {
                cached
            } else {
                // Cache miss — ask PHP-FPM to run the auth chain.
                match nc_fastcgi::resolve_session(fpm, &cookie_header).await {
                    Some(result) => {
                        nc_auth::cache_insert(session_cache, cache_key, result.identity.clone());
                        // Remember-me token rotation: forward any
                        // Set-Cookie headers the shim emitted so the
                        // browser receives the refreshed nc_token /
                        // nc_username / nc_session_id cookies (§7.9.3).
                        pending_set_cookies = result.set_cookies;
                        result.identity
                    }
                    None => {
                        // PHP says the session is invalid or unauthenticated.
                        // Fall through as anonymous — the route handler
                        // decides whether to reject with 401.
                        return next.run(req).await;
                    }
                }
            };

            // ── DAV session-fixation guard (§7.9.6; Auth.php:184-186) ───────
            //
            // PHP's Auth.php accepts a cookie-only (session-based) DAV request
            // when:
            //   1. AUTHENTICATED_TO_DAV_BACKEND is null (first DAV request in
            //      the session — "Fix for broken webdav clients", Auth.php:184)
            //   2. AUTHENTICATED_TO_DAV_BACKEND === current UID AND the
            //      Authorization header is absent (well-behaved client,
            //      Auth.php:186)
            // Any other case (DAV_AUTHENTICATED stores a *different* UID) is a
            // session-fixation attempt and must be rejected with 401.
            //
            // This check is scoped to DAV endpoint paths only; OCS and other
            // routes do not run through SabreDAV Auth.php.
            if is_dav
                && !dav_session_guard(&identity.uid, identity.dav_authenticated_uid.as_deref())
            {
                return build_401(true, is_xhr, true, false);
            }

            // Build and return the resolved identity.
            let is_admin =
                nc_auth::is_admin_user(&identity.uid, &state.pool, &state.table_prefix).await;
            Some(AuthInfo {
                uid: identity.uid,
                is_admin,
                method: AuthMethod::Session,
                token_id: None,
                raw_token: None,
            })
        }
    };

    // Attach identity (or nothing) to request extensions.
    if let Some(info) = auth_info {
        req.extensions_mut().insert(info);
    }

    let mut resp = next.run(req).await;

    // Forward any Set-Cookie headers from the PHP-FPM session resolver.
    // These originate from the remember-me token rotation path
    // (`loginWithCookie()` → `setMagicInCookie()` — §7.9.3/§7.9.4) and
    // carry the refreshed nc_token / nc_username / nc_session_id cookies
    // that the browser must receive to stay logged in.
    for cookie_val in &pending_set_cookies {
        if let Ok(hv) = axum::http::HeaderValue::from_str(cookie_val) {
            resp.headers_mut()
                .append(axum::http::header::SET_COOKIE, hv);
        }
    }

    resp
}

// ── Response helpers ──────────────────────────────────────────────────────────

/// Build a 401 response with the correct `WWW-Authenticate` header.
///
/// REQ §4.3 (DAV): `Basic realm="Nextcloud"` or `DummyBasic realm="Nextcloud"` for XHR.
/// REQ §5.4 (OCS): `Basic realm="Authorisation Required"` or `DummyBasic …` for XHR.
///
/// `send_www_auth` — whether to emit the header at all (false for bearer rejections).
/// `is_xhr`        — `X-Requested-With: XMLHttpRequest` was present (DummyBasic).
/// `is_dav`        — request is for a DAV endpoint (realm = "Nextcloud").
/// `is_2fa`        — response body should carry the 2FA message (unused here;
///                   2FA rejections bypass this function and return inline).
fn build_401(send_www_auth: bool, is_xhr: bool, is_dav: bool, _is_2fa: bool) -> Response {
    let www_auth = if !send_www_auth {
        None
    } else {
        let realm = if is_dav {
            "Nextcloud"
        } else {
            "Authorisation Required"
        };
        let scheme = if is_xhr { "DummyBasic" } else { "Basic" };
        Some(format!("{scheme} realm=\"{realm}\""))
    };

    let mut builder = axum::http::Response::builder().status(StatusCode::UNAUTHORIZED);
    if let Some(wa) = www_auth {
        builder = builder.header("WWW-Authenticate", wa);
    }
    builder
        .body(Body::from("Unauthorized"))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Build a 429 Too Many Requests response (REQ §4.6).
fn build_429(retry_after_secs: u64) -> Response {
    axum::http::Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("Retry-After", retry_after_secs.to_string())
        .body(Body::from("Too Many Requests"))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// DAV session-fixation guard (§7.9.6 / `Auth.php:184-186`).
///
/// Extracted as a standalone function solely to allow unit testing without a
/// live `AppState` or PHP-FPM socket — the logic is otherwise a 3-line match
/// used exactly once in `auth_layer`.
///
/// Returns `true` when the request should be accepted, `false` when it must
/// be rejected with `401`.
///
/// Three cases (matching `Auth.php` exactly):
/// - `dav_authenticated_uid == None`        → accept (first DAV request in session;
///                                            "Fix for broken webdav clients", `Auth.php:184`)
/// - `dav_authenticated_uid == Some(uid)`   → accept (well-behaved cookie-only
///                                            client, `Auth.php:186`)
/// - `dav_authenticated_uid == Some(other)` → reject (session fixation: the
///                                            session was previously associated
///                                            with a different user)
pub(crate) fn dav_session_guard(uid: &str, dav_authenticated_uid: Option<&str>) -> bool {
    match dav_authenticated_uid {
        None => true,
        Some(dav_uid) => dav_uid == uid,
    }
}

/// Extract the real client IP, honouring `X-Forwarded-For` if present.
fn extract_client_ip(req: &Request<Body>) -> String {
    req.headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .unwrap_or("127.0.0.1")
        .to_string()
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, middleware, routing::get, Router};
    use nc_db::{
        appconfig::AppConfigCache,
        config::{DbType, NcConfig},
        mime::MimeCache,
    };
    use std::sync::{Arc, RwLock};
    use tower::ServiceExt;

    /// Build a minimal `AppState` backed by an in-memory SQLite pool.
    ///
    /// `fastcgi` and `session_cache` are left as `None` — these tests exercise
    /// the middleware paths that do not require a live PHP-FPM connection.
    async fn make_test_state(instanceid: &str) -> AppState {
        sqlx::any::install_default_drivers();
        let pool = sqlx::AnyPool::connect("sqlite::memory:").await.unwrap();
        let appconfig_cache = Arc::new(RwLock::new(AppConfigCache::default()));
        let capability_cache = nc_ocs::load_capability_cache(&appconfig_cache);
        let nc_config = NcConfig {
            dbtype: DbType::Sqlite,
            dbhost: None,
            dbname: None,
            dbuser: None,
            dbpassword: None,
            dbtableprefix: "oc_".to_owned(),
            datadirectory: None,
            instanceid: Some(instanceid.to_owned()),
            installed: true,
            maintenance: false,
            version: None,
            trusted_domains: None,
            overwrite_cli_url: None,
            bruteforce_protection_enabled: false,
            oauth2_enable_oc_clients: false,
            memcache_distributed: None,
            loglevel: 1,
            logfile: None,
            data_fingerprint: None,
            bulkupload_enabled: true,
            enable_previews: true,
            preview_ffmpeg_path: None,
            preview_libreoffice_path: None,
            enabled_preview_providers: None,
            preview_imaginary_url: None,
            fastcgi_socket: None,
            fastcgi_timeout_ms: 30_000,
            forbidden_filenames: vec![".htaccess".to_owned()],
            forbidden_filename_basenames: vec![],
            forbidden_filename_characters: vec![],
            forbidden_filename_extensions: vec![".filepart".to_owned()],
        };
        let preview_registry = Arc::new(nc_dav::ProviderRegistry::from_config(&nc_config));
        AppState {
            pool,
            mime_cache: Arc::new(RwLock::new(MimeCache::default())),
            appconfig_cache,
            capability_cache,
            token_cache: nc_auth::new_token_cache(),
            nc_config: Arc::new(nc_config),
            nc_root: std::path::PathBuf::from("."),
            table_prefix: "oc_".to_owned(),
            fastcgi: None,
            instanceid: instanceid.to_owned(),
            session_cache: None,
            upload_state_store: Arc::new(nc_dav::UploadStateStore::new()),
            preview_registry,
        }
    }

    async fn ok_handler() -> StatusCode {
        StatusCode::OK
    }

    /// No `{instanceid}` cookie and no `nc_token` → the middleware treats the
    /// request as anonymous (no `AuthInfo` extension) and passes it through
    /// without a 401.  The downstream handler decides whether auth is required.
    ///
    /// This verifies the `_ =>` arm's first guard: absent session cookies →
    /// anonymous, not a 401.
    #[tokio::test]
    async fn session_no_cookies_is_anonymous() {
        let state = make_test_state("oc1abc").await;
        let app = Router::new()
            .route("/dav/files/alice", get(ok_handler))
            .layer(middleware::from_fn_with_state(state.clone(), auth_layer));

        let req = Request::builder()
            .method("GET")
            .uri("/dav/files/alice")
            // Deliberately empty Cookie header — no session cookies.
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        // Middleware passes through; handler returns 200 OK.
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── dav_session_guard pure-function tests ────────────────────────────────
    //
    // These cover the DAV session-fixation guard without any AppState or
    // network calls — the function is pure so no async machinery is needed.

    /// `AUTHENTICATED_TO_DAV_BACKEND` absent (first DAV request in session).
    /// PHP `Auth.php:184`: "Fix for broken webdav clients" — always accept.
    #[test]
    fn dav_guard_first_request_accepted() {
        assert!(dav_session_guard("alice", None));
    }

    /// `AUTHENTICATED_TO_DAV_BACKEND` matches the resolved UID.
    /// PHP `Auth.php:186`: well-behaved cookie-only client — accept.
    #[test]
    fn dav_guard_uid_match_accepted() {
        assert!(dav_session_guard("alice", Some("alice")));
    }

    /// `AUTHENTICATED_TO_DAV_BACKEND` stores a *different* UID.
    /// Session-fixation attempt — guard returns false → 401.
    #[test]
    fn dav_guard_uid_mismatch_rejected() {
        assert!(!dav_session_guard("alice", Some("bob")));
    }

    // ── Additional middleware tests ───────────────────────────────────────────

    /// `OCS-APIRequest: true` causes the SameSite guard to be skipped entirely
    /// (`Request.php:464-468`).  A session cookie present with missing guard
    /// cookies must NOT return 412 when `OCS-APIRequest` is set.
    ///
    /// With `fastcgi: None` the auth resolves to anonymous (200), confirming
    /// the 412 path was bypassed.
    #[tokio::test]
    async fn session_ocs_apirequest_bypasses_samesite() {
        let state = make_test_state("oc1abc").await;
        let app = Router::new()
            .route("/ocs/v2.php/cloud/capabilities", get(ok_handler))
            .layer(middleware::from_fn_with_state(state.clone(), auth_layer));

        // Session cookie present but SameSite guard cookies absent — would be
        // 412 without the OCS-APIRequest bypass.
        let req = Request::builder()
            .method("GET")
            .uri("/ocs/v2.php/cloud/capabilities")
            .header("cookie", "oc1abc=somesessionid")
            .header("OCS-APIRequest", "true")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// When `fastcgi` is `None`, session cookies are present and the SameSite
    /// guard passes — the middleware has no resolver to call and must fall
    /// through as anonymous (not 401, not 412).
    #[tokio::test]
    async fn session_no_fpm_is_anonymous() {
        let state = make_test_state("oc1abc").await;
        let app = Router::new()
            .route("/dav/files/alice", get(ok_handler))
            .layer(middleware::from_fn_with_state(state.clone(), auth_layer));

        // Valid session cookie + guard cookies, but fastcgi = None.
        let req = Request::builder()
            .method("GET")
            .uri("/dav/files/alice")
            .header(
                "cookie",
                "oc1abc=somesessionid; nc_sameSiteCookielax=true; nc_sameSiteCookiestrict=true",
            )
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        // No FastCGI resolver → anonymous → downstream handler returns 200.
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// When the PHP session cookie (`{instanceid}`) is present but the
    /// SameSite guard cookies (`nc_sameSiteCookielax`, `nc_sameSiteCookiestrict`)
    /// are absent, PHP returns HTTP 412 Precondition Failed (base.php strict
    /// cookie check — REQ §7.9.2 / §7.9.6).
    ///
    /// The Rust middleware must match that behavior — not 401, not 200.
    #[tokio::test]
    async fn session_samesite_failure_returns_412() {
        let state = make_test_state("oc1abc").await;
        let app = Router::new()
            .route("/remote.php/webdav/", get(ok_handler))
            .layer(middleware::from_fn_with_state(state.clone(), auth_layer));

        // `oc1abc` session cookie is present (triggers the SameSite check) but
        // neither guard cookie is present → `StrictCheckFailed` → 412.
        let req = Request::builder()
            .method("GET")
            .uri("/remote.php/webdav/")
            .header("cookie", "oc1abc=somesessionid")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    }
}
