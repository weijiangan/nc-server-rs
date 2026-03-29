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
    path.starts_with("/remote.php")
        || path.starts_with("/dav")
        || path.starts_with("/public.php")
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
        match check_samesite_cookies(&cookie_header, is_https) {
            CookieCheck::StrictCheckFailed => {
                return (StatusCode::UNAUTHORIZED, "SameSite cookie check failed")
                    .into_response();
            }
            _ => {}
        }
    }

    // ── Credential extraction and verification ────────────────────────────
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
                    let is_admin = nc_auth::is_admin_user(
                        &cached.uid,
                        &state.pool,
                        &state.table_prefix,
                    )
                    .await;

                    // Extract the raw bearer value for HTTP_X_NC_SESSION_TOKEN.
                    let raw_token = nc_auth::bearer::extract_bearer(ah)
                        .map(str::to_owned);

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
                                nc_auth::basic::extract_basic(ah)
                                    .map(|(_, pwd)| pwd)
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
        _ => None, // No Authorization header — anonymous request.
    };

    // Attach identity (or nothing) to request extensions.
    if let Some(info) = auth_info {
        req.extensions_mut().insert(info);
    }

    next.run(req).await
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
