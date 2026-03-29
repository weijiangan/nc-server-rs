/// Session-cookie based authentication.
///
/// ## Current status
/// PHP-native session storage (file-based PHP sessions) cannot be read by Rust
/// without implementing a PHP session deserialiser or sharing a session store.
/// Full session auth will be wired in **Phase 7** once PHP-FPM is proxied and
/// session identity can be injected via FastCGI params.
///
/// ### What this module validates now
/// - `nc_sameSiteCookiestrict=true` and `nc_sameSiteCookielax=true` must both
///   be present if `nc_session_id` or `nc_token` is present (strict cookie
///   check, API_COMPATIBILITY.md §CSRF).
/// - If neither session cookie is present, validation is skipped entirely.
///
/// ### What is NOT implemented yet
/// - Actual PHP session file / Redis lookup → uid resolution.
/// - `AUTHENTICATED_TO_DAV_BACKEND` session key check.

/// Result of the strict-cookie guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieCheck {
    /// Neither `nc_session_id` nor `nc_token` is present — nothing to validate.
    NoSessionCookies,
    /// Session cookie present and SameSite guard cookies also present.
    Valid,
    /// Session cookie present but SameSite guard cookies missing — reject.
    StrictCheckFailed,
}

/// Run the SameSite guard (strict cookie check).
///
/// `cookies` is the raw value of the `Cookie:` header.
pub fn check_samesite_cookies(cookies: &str, is_https: bool) -> CookieCheck {
    let has_session = cookie_value(cookies, "nc_session_id").is_some()
        || cookie_value(cookies, "nc_token").is_some();

    if !has_session {
        return CookieCheck::NoSessionCookies;
    }

    // On HTTPS, the SameSite cookies have the __Host- prefix.
    let (lax_key, strict_key) = if is_https {
        ("__Host-nc_sameSiteCookielax", "__Host-nc_sameSiteCookiestrict")
    } else {
        ("nc_sameSiteCookielax", "nc_sameSiteCookiestrict")
    };

    let lax_ok = cookie_value(cookies, lax_key)
        .map(|v| v == "true")
        .unwrap_or(false);
    let strict_ok = cookie_value(cookies, strict_key)
        .map(|v| v == "true")
        .unwrap_or(false);

    if lax_ok && strict_ok {
        CookieCheck::Valid
    } else {
        CookieCheck::StrictCheckFailed
    }
}

/// Extract all `nc_session_id` / `nc_token` cookie values (returns at most one).
pub fn session_cookie_value<'a>(cookies: &'a str) -> Option<&'a str> {
    cookie_value(cookies, "nc_session_id").or_else(|| cookie_value(cookies, "nc_token"))
}

/// Minimal cookie-string parser: find the value for a given key.
fn cookie_value<'a>(cookies: &'a str, name: &str) -> Option<&'a str> {
    for pair in cookies.split(';') {
        let pair = pair.trim();
        if let Some(rest) = pair.strip_prefix(name) {
            if let Some(value) = rest.strip_prefix('=') {
                return Some(value.trim());
            }
        }
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_session_cookies() {
        assert_eq!(check_samesite_cookies("foo=bar", false), CookieCheck::NoSessionCookies);
    }

    #[test]
    fn strict_check_passes_http() {
        let cookies =
            "nc_session_id=abc; nc_sameSiteCookielax=true; nc_sameSiteCookiestrict=true";
        assert_eq!(check_samesite_cookies(cookies, false), CookieCheck::Valid);
    }

    #[test]
    fn strict_check_fails_missing_guard() {
        let cookies = "nc_session_id=abc; nc_sameSiteCookielax=true";
        assert_eq!(
            check_samesite_cookies(cookies, false),
            CookieCheck::StrictCheckFailed
        );
    }

    #[test]
    fn strict_check_passes_https_with_host_prefix() {
        let cookies =
            "nc_session_id=abc; __Host-nc_sameSiteCookielax=true; __Host-nc_sameSiteCookiestrict=true";
        assert_eq!(check_samesite_cookies(cookies, true), CookieCheck::Valid);
    }

    #[test]
    fn cookie_value_extraction() {
        let cookies = "a=1; b=hello; c=";
        assert_eq!(cookie_value(cookies, "a"), Some("1"));
        assert_eq!(cookie_value(cookies, "b"), Some("hello"));
        assert_eq!(cookie_value(cookies, "c"), Some(""));
        assert_eq!(cookie_value(cookies, "d"), None);
    }
}
