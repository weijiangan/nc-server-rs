use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};

use nc_auth::{AuthInfo, AuthMethod};

/// How long a request waits for a `__session_resolve` concurrency permit
/// before falling through anonymous (F3, Wave 2.1).  The resolution itself
/// carries a 5 s timeout in `nc_fastcgi::resolve_session`; this bound is
/// about *queueing*, not the round-trip — under saturation, excess requests
/// degrade to anonymous quickly instead of piling onto FPM.
const RESOLVE_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

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

/// Auth check (Phase 3), called from `router::http_middleware_stack` — the
/// composite that runs static files → maintenance → auth inside ONE `from_fn`
/// layer (Phase 18.6).  Executed AFTER the maintenance check.
///
/// Behaviour per request:
/// - Extract credentials from `Authorization` header (Bearer → Basic priority).
/// - If valid credentials: attach `AuthInfo` extension; update last_activity.
/// - If invalid credentials: return `Err(401)` immediately (brute-force recorded).
/// - If no credentials: anonymous — no extension attached; handler decides
///   whether to require auth.
///
/// `Ok(Vec<String>)` carries the session resolver's Set-Cookie values
/// (remember-me rotation) for the composite to append after the handler.
#[tracing::instrument(skip_all, level = "debug", fields(method = %req.method(), path = %req.uri().path()))]
pub async fn auth_check(
    state: &AppState,
    req: &mut Request<Body>,
) -> Result<Vec<String>, Response> {
    let path = req.uri().path().to_string();
    let _method = req.method().as_str().to_string();

    // Skip auth processing for /status.php and /heartbeat entirely.
    if matches!(path.as_str(), "/status.php" | "/heartbeat") {
        return Ok(Vec::new());
    }

    // ── Header extraction ─────────────────────────────────────────────────
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Diagnostic: log whether we have credentials and their type (first 6 chars).
    // Auth failures are otherwise silent — this is the only signal in debug logs.
    match &auth_header {
        Some(ah) if ah.len() >= 6 => {
            tracing::debug!(auth_prefix = %&ah[..6], path = %path, "auth header present");
        }
        Some(_) => {
            tracing::debug!(auth_prefix = "???", path = %path, "auth header too short");
        }
        None => {
            tracing::debug!(path = %path, "no Authorization header");
        }
    }

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
    // Phase 15 F2: the composite resolved the client identity (trusted-proxy
    // XFF walk); the throttle key must use it, not the raw header.
    let client_ip = match req
        .extensions()
        .get::<crate::client_identity::ClientIdentity>()
    {
        Some(identity) => identity.ip.to_string(),
        None => {
            tracing::warn!("auth_check without a resolved ClientIdentity extension");
            "127.0.0.1".to_string()
        }
    };

    // REQ §18: `secret` is a system config key in `config.php`, NOT an
    // app config — it does not live in `oc_appconfig`.  Token hashing uses
    // `hash('sha512', $token . $secret)` (`PublicKeyTokenProvider.php:414`)
    // so an empty secret would produce a different hash than PHP and cause
    // every app-token / device-token auth to fail with 401.
    let app_secret = state.nc_config.secret.as_deref().unwrap_or_default();
    // `passwordsalt` — only used by the hasher's legacy (pre-versioning) path;
    // empty matches PHP's `getSystemValue('passwordsalt', '')` default.
    let legacy_salt = state.nc_config.passwordsalt.as_deref().unwrap_or_default();

    // ── CSRF check ────────────────────────────────────────────────────────
    // For Phase 3, we only enforce CSRF if session cookies are present AND
    // the strict cookie guard fails (task 3.5).
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Phase 15 F2: scheme from the resolved identity (X-Forwarded-Proto is
    // honoured only from trusted proxies).
    let is_https = req
        .extensions()
        .get::<crate::client_identity::ClientIdentity>()
        .map(|i| i.https)
        .unwrap_or(false);

    use nc_auth::session::{check_samesite_cookies, CookieCheck};
    if auth_header.is_none() && !ocs_api_request {
        match check_samesite_cookies(&cookie_header, &state.instanceid, is_https) {
            CookieCheck::StrictCheckFailed => {
                // PHP returns 412 Precondition Failed for strict cookie check
                // failures (base.php strict cookie check, REQ §7.9.2).
                return Err((
                    StatusCode::PRECONDITION_FAILED,
                    "SameSite cookie check failed",
                )
                    .into_response());
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
                return Err(build_429(throttle.retry_after_secs));
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
                    // Fire-and-forget last_activity update (task 3.4),
                    // throttled to PHP's 60 s interval (round-4 Task 11).
                    nc_auth::bearer::spawn_last_activity_update(
                        cached.id,
                        cached.last_activity,
                        state.pool.clone(),
                        state.table_prefix.clone(),
                    );

                    // 2FA gate (task 3.6) — REQ §4.5.  Round-3 Task 8: the
                    // provider check and the admin-group check are cached per
                    // uid (60 s TTL); permanent app tokens are exempt from
                    // the 2FA gate.
                    let user_state =
                        nc_auth::cached_user_state(&cached.uid, &state.pool, &state.table_prefix)
                            .await;
                    let user_state = match user_state {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(
                                uid = %cached.uid,
                                error = %e,
                                "2FA check query failed — returning 500"
                            );
                            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
                        }
                    };
                    if cached.token_type != 1 && user_state.twofa_enabled {
                        return Err((
                            StatusCode::UNAUTHORIZED,
                            "Not Authenticated: 2FA challenge not passed.",
                        )
                            .into_response());
                    }

                    // Phase 7.2: admin group check (cached per uid).
                    let is_admin = user_state.is_admin;

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
                    tracing::debug!(path = %path, "bearer token lookup failed");
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
                    return Err(build_401(send_www_auth, is_xhr, is_dav, false));
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
                return Err(build_429(throttle.retry_after_secs));
            }
            if let Some(delay) = throttle.delay {
                tokio::time::sleep(delay).await;
            }

            match nc_auth::basic::extract_basic(ah) {
                None => {
                    tracing::debug!(path = %path, "Basic auth header failed to decode (base64/UTF-8/colon)");
                    return Err(build_401(true, is_xhr, is_dav, false));
                }
                Some((login, password)) => {
                    // REQ §4.1/4.2: try app token first, then plain password.
                    let secret_len = app_secret.len();
                    tracing::debug!(
                        login_len = login.len(),
                        password_len = password.len(),
                        secret_len,
                        path = %path,
                        "attempting Basic auth verification"
                    );
                    match nc_auth::basic::verify_basic(
                        &login,
                        &password,
                        &state.pool,
                        &state.table_prefix,
                        &app_secret,
                        &legacy_salt,
                    )
                    .await
                    {
                        Some(result) => {
                            tracing::debug!(
                                uid = %result.uid,
                                is_app_token = result.token_id.is_some(),
                                "Basic auth succeeded"
                            );
                            // 2FA gate for Basic auth (REQ §4.5).  Round-3
                            // Task 8: both the provider check and the
                            // admin-group check are cached per uid.
                            let token_type = result.token_type.unwrap_or(0);
                            let user_state = nc_auth::cached_user_state(
                                &result.uid,
                                &state.pool,
                                &state.table_prefix,
                            )
                            .await;
                            let user_state = match user_state {
                                Ok(s) => s,
                                Err(e) => {
                                    tracing::error!(
                                        uid = %result.uid,
                                        error = %e,
                                        "2FA check query failed — returning 500"
                                    );
                                    return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
                                }
                            };
                            if token_type != 1 && user_state.twofa_enabled {
                                return Err((
                                    StatusCode::UNAUTHORIZED,
                                    "Not Authenticated: 2FA challenge not passed.",
                                )
                                    .into_response());
                            }
                            // Phase 7.2: admin group check (cached per uid).
                            let is_admin = user_state.is_admin;
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
                            tracing::debug!(
                                login_len = login.len(),
                                secret_len,
                                path = %path,
                                "Basic auth failed — token hash mismatch and password-hash mismatch (or DB error)"
                            );
                            nc_auth::bruteforce::record_attempt(
                                "login",
                                &client_ip,
                                &state.pool,
                                &state.table_prefix,
                            )
                            .await;
                            return Err(build_401(true, is_xhr, is_dav, false));
                        }
                    }
                }
            }
        }
        _ => {
            // No Authorization header (or unrecognized scheme) — try PHP session
            // cookie resolution (§7.9.6).
            tracing::debug!(
                path = %path,
                has_cookies = !cookie_header.is_empty(),
                "no recognized Authorization header — falling back to session cookies"
            );
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
                return Ok(Vec::new());
            };

            // Find the raw session-cookie value (PHP session cookie keyed on
            // `{instanceid}`, or `nc_token` for the remember-me fallback).
            // PHP's `cookieCheckRequired()` uses the same two cookies as the
            // session trigger (Request.php:464-470).
            let Some(raw_val) =
                nc_auth::session::session_cookie_value(&state.instanceid, &cookie_header)
            else {
                // No session cookies at all — anonymous request.
                return Ok(Vec::new());
            };
            let raw_val = raw_val.to_owned();

            // ── Session identity cache lookup (§7.9.5) ──────────────────────
            // Key: SHA-256(raw PHP session cookie value).
            let cache_key = nc_auth::make_cache_key(&raw_val);
            let positive_ttl =
                std::time::Duration::from_secs(state.nc_config.session_cache_ttl.unwrap_or(60));
            let identity = match nc_auth::cache_lookup(session_cache, &cache_key, positive_ttl) {
                nc_auth::CacheLookup::Positive(cached) => cached,
                // F3 (Wave 2.1): a fresh negative entry — a junk cookie or
                // expired session resolved within the last 5 s — is treated
                // exactly like a fresh failed resolution (anonymous) without
                // touching PHP-FPM.  This is what absorbs an attacker's
                // request burst.
                nc_auth::CacheLookup::Negative => return Ok(Vec::new()),
                nc_auth::CacheLookup::Miss => {
                    // F3 concurrency cap: at most `session_resolve_concurrency`
                    // `__session_resolve` round-trips in flight, so FPM
                    // saturation is not reachable through resolution alone.
                    // Excess requests wait a bounded time, then fall through
                    // anonymous (the handler may still 401).
                    let sem = fpm.session_resolve_semaphore.clone();
                    match tokio::time::timeout(RESOLVE_ACQUIRE_TIMEOUT, sem.acquire_owned()).await {
                        Ok(Ok(_permit)) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "session-resolve: semaphore closed");
                            return Ok(Vec::new());
                        }
                        Err(_) => {
                            tracing::warn!(
                                "session-resolve: concurrency cap busy for {RESOLVE_ACQUIRE_TIMEOUT:?} — \
                                 falling through anonymous"
                            );
                            return Ok(Vec::new());
                        }
                    }
                    // Cache miss — ask PHP-FPM to run the auth chain.
                    match nc_fastcgi::resolve_session(fpm, &cookie_header).await {
                        Some(result) => {
                            nc_auth::cache_insert(
                                session_cache,
                                cache_key,
                                result.identity.clone(),
                            );
                            // Remember-me token rotation: forward any
                            // Set-Cookie headers the shim emitted so the
                            // browser receives the refreshed nc_token /
                            // nc_username / nc_session_id cookies (§7.9.3).
                            //
                            // NOTE (F3): rotation runs on cache miss only —
                            // a cached identity skips PHP's
                            // loginWithCookie(), so remember-me tokens
                            // rotate at most once per positive-TTL window,
                            // not per request.  Deliberate: the cache TTL
                            // (the revocation knob) bounds it.
                            pending_set_cookies = result.set_cookies;
                            result.identity
                        }
                        None => {
                            // PHP says the session is invalid or
                            // unauthenticated.  Cache the negative result
                            // (5 s TTL) so a burst of junk cookies hits
                            // memory, not FPM (F3) — and fall through as
                            // anonymous; the route handler decides whether
                            // to reject with 401.
                            nc_auth::cache_insert_negative(session_cache, cache_key);
                            return Ok(Vec::new());
                        }
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
                return Err(build_401(true, is_xhr, true, false));
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

    // Return the session resolver's Set-Cookie values (remember-me token
    // rotation — §7.9.3/§7.9.4); `http_middleware_stack` forwards them onto
    // the response after the handler runs, exactly as the former middleware
    // did.
    Ok(pending_set_cookies)
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
/// used exactly once in `auth_check`.
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, middleware, routing::get, Router};
    use nc_db::{
        appconfig::AppConfigCache,
        config::{DbType, NcConfig},
        mime::MimeCache,
        pool::DbPool,
    };
    use std::sync::{Arc, RwLock};
    use tower::ServiceExt;

    /// Build a minimal `AppState` backed by an in-memory SQLite pool.
    ///
    /// `fastcgi` and `session_cache` are left as `None` — these tests exercise
    /// the middleware paths that do not require a live PHP-FPM connection.
    async fn make_test_state(instanceid: &str) -> AppState {
        make_test_state_with_root(instanceid, std::path::PathBuf::from(".")).await
    }

    /// Same, with an explicit `nc_root` (the static-file whitelist resolves
    /// candidates against it).
    async fn make_test_state_with_root(instanceid: &str, nc_root: std::path::PathBuf) -> AppState {
        let pool = DbPool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        let appconfig_cache = Arc::new(RwLock::new(AppConfigCache::default()));
        let capability_cache = nc_ocs::load_capability_cache(&appconfig_cache, None);
        let nc_config = NcConfig {
            dbtype: DbType::Sqlite,
            dbhost: None,
            dbname: None,
            dbuser: None,
            dbpassword: None,
            dbtableprefix: "oc_".to_owned(),
            datadirectory: None,
            instanceid: Some(instanceid.to_owned()),
            serverid: None,
            installed: true,
            maintenance: false,
            secret: None,
            passwordsalt: None,
            version: None,
            trusted_domains: None,
            overwrite_cli_url: None,
            trusted_proxies: None,
            forwarded_for_headers: None,
            overwritehost: None,
            overwriteprotocol: None,
            overwritecondaddr: None,
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
            preview_imaginary_key: None,
            preview_format: None,
            preview_max_x: None,
            preview_max_y: None,
            preview_max_filesize_image: None,
            preview_concurrency_new: None,
            preview_concurrency_all: None,
            fastcgi_socket: None,
            fastcgi_timeout_ms: 30_000,
            session_cache_ttl: None,
            session_resolve_concurrency: None,
            php_binary: None,
            forbidden_filenames: vec![".htaccess".to_owned()],
            forbidden_filename_basenames: vec![],
            forbidden_filename_characters: vec![],
            forbidden_filename_extensions: vec![".filepart".to_owned()],
        };
        let preview_registry = Arc::new(nc_dav::ProviderRegistry::from_config(&nc_config));
        let preview_gen = crate::preview_gen::PreviewGen::from_config(
            &nc_config,
            &appconfig_cache,
            &preview_registry,
        );
        AppState {
            pool,
            mime_cache: Arc::new(RwLock::new(MimeCache::default())),
            dir_mime_id: 2,
            dir_mimepart_id: 1,
            lazy_cache_ensured: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            storage_cache: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            appconfig_cache,
            capability_cache,
            token_cache: nc_auth::new_token_cache(),
            nc_config: Arc::new(nc_config),
            nc_root,
            table_prefix: "oc_".to_owned(),
            fastcgi: None,
            instanceid: instanceid.to_owned(),
            session_cache: None,
            upload_state_store: Arc::new(nc_dav::UploadStateStore::new()),
            preview_registry,
            preview_gen,
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
            .layer(middleware::from_fn_with_state(
                state.clone(),
                crate::router::http_middleware_stack,
            ));

        let mut req = Request::builder()
            .method("GET")
            .uri("/dav/files/alice")
            // Deliberately empty Cookie header — no session cookies.
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:443".parse::<std::net::SocketAddr>().unwrap(),
        ));

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
            .layer(middleware::from_fn_with_state(
                state.clone(),
                crate::router::http_middleware_stack,
            ));

        // Session cookie present but SameSite guard cookies absent — would be
        // 412 without the OCS-APIRequest bypass.
        let mut req = Request::builder()
            .method("GET")
            .uri("/ocs/v2.php/cloud/capabilities")
            .header("cookie", "oc1abc=somesessionid")
            .header("OCS-APIRequest", "true")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:443".parse::<std::net::SocketAddr>().unwrap(),
        ));

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
            .layer(middleware::from_fn_with_state(
                state.clone(),
                crate::router::http_middleware_stack,
            ));

        // Valid session cookie + guard cookies, but fastcgi = None.
        let mut req = Request::builder()
            .method("GET")
            .uri("/dav/files/alice")
            .header(
                "cookie",
                "oc1abc=somesessionid; nc_sameSiteCookielax=true; nc_sameSiteCookiestrict=true",
            )
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:443".parse::<std::net::SocketAddr>().unwrap(),
        ));

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
            .layer(middleware::from_fn_with_state(
                state.clone(),
                crate::router::http_middleware_stack,
            ));

        // `oc1abc` session cookie is present (triggers the SameSite check) but
        // neither guard cookie is present → `StrictCheckFailed` → 412.
        let mut req = Request::builder()
            .method("GET")
            .uri("/remote.php/webdav/")
            .header("cookie", "oc1abc=somesessionid")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:443".parse::<std::net::SocketAddr>().unwrap(),
        ));

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    }

    // ── Static-file whitelist deny-path pins (15.1 remainder) ────────────────
    //
    // The `try_static_files_check` whitelist (`/core/ /dist/ /themes/ /apps/`
    // + `robots.txt` + `index.html`, Phase 18.1) is the F1 static-serving
    // control: paths outside it never reach the filesystem.  These tests pin
    // that the deny side holds even when a real file exists at the denied
    // path — the property is whitelist-first, not "check the filesystem".

    /// Scratch root with the given (relative path → content) files; removed on
    /// drop.  Unique per test+process so parallel runs cannot collide.
    struct ScratchRoot(std::path::PathBuf);
    impl ScratchRoot {
        fn new(name: &str, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!("nc-static-{}-{name}", std::process::id()));
            for (rel, content) in files {
                let p = dir.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, content).unwrap();
            }
            ScratchRoot(dir)
        }
    }
    impl Drop for ScratchRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// GET `uri` through the composite middleware with `root` as `nc_root`;
    /// returns the final status.
    async fn static_request(root: &ScratchRoot, uri: &str) -> StatusCode {
        let state = make_test_state_with_root("oc1abc", root.0.clone()).await;
        let app = Router::new()
            .route("/dav/files/alice", get(ok_handler))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                crate::router::http_middleware_stack,
            ));
        let mut req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:443".parse::<std::net::SocketAddr>().unwrap(),
        ));
        app.oneshot(req).await.unwrap().status()
    }

    /// `/data/` is not a whitelisted root — `.ocdata` (or any file) under it
    /// is never served as static, even when present on disk.
    #[tokio::test]
    async fn static_denies_data_directory() {
        let root = ScratchRoot::new("data-dir", &[("data/.ocdata", "x")]);
        assert_eq!(
            static_request(&root, "/data/.ocdata").await,
            StatusCode::NOT_FOUND
        );
    }

    /// Root-level dotfiles (`.htaccess`, `.user.ini`, …) are not whitelisted
    /// and are never served, even when present on disk.
    #[tokio::test]
    async fn static_denies_dotfile_path() {
        let root = ScratchRoot::new("dotfile", &[(".htaccess", "x")]);
        assert_eq!(
            static_request(&root, "/.htaccess").await,
            StatusCode::NOT_FOUND
        );
    }

    /// `3rdparty/` (PHP's bundled libraries) is not a whitelisted root — a
    /// real file under it is still denied; real nginx installs deny it too.
    #[tokio::test]
    async fn static_denies_3rdparty() {
        let root = ScratchRoot::new("thirdparty", &[("3rdparty/autoload.php", "x")]);
        assert_eq!(
            static_request(&root, "/3rdparty/autoload.php").await,
            StatusCode::NOT_FOUND
        );
    }

    /// The whitelist is case-sensitive (like nginx's literal `try_files`):
    /// `/CORE/…` does NOT match `/core/…` and is denied even when the file
    /// exists on disk (a case-insensitive read would serve it).
    #[tokio::test]
    async fn static_denied_prefix_case_insensitive() {
        let root = ScratchRoot::new("case", &[("CORE/img/logo.svg", "x")]);
        assert_eq!(
            static_request(&root, "/CORE/img/logo.svg").await,
            StatusCode::NOT_FOUND
        );
    }

    /// Traversal segments inside a whitelisted root are rejected before the
    /// fs stat (`path.contains("..")`), so `/core/../…` cannot escape.
    #[tokio::test]
    async fn static_denies_traversal_segment() {
        let root = ScratchRoot::new("traversal", &[("data/.ocdata", "x")]);
        assert_eq!(
            static_request(&root, "/core/../../data/.ocdata").await,
            StatusCode::NOT_FOUND
        );
    }

    /// Positive control: a real file under a whitelisted root IS served — the
    /// whitelist gates, it does not disable static serving entirely.
    #[tokio::test]
    async fn static_serves_whitelisted_asset() {
        let root = ScratchRoot::new("serves", &[("core/img/logo.svg", "<svg/>")]);
        assert_eq!(
            static_request(&root, "/core/img/logo.svg").await,
            StatusCode::OK
        );
    }
}
