/// Rules for determining whether a CSRF check should be skipped.
///
/// Implements REQ §4.3 and §4.4.
///
/// ### Skip conditions (in priority order)
/// 1. Read-only HTTP method (GET, HEAD, OPTIONS)
/// 2. `OCS-APIREQUEST: true` header present
/// 3. Any `Authorization` header present (Bearer / Basic — non-session auth)
/// 4. Known desktop or mobile sync-client User-Agent (exact regexes from REQ §4.4)
///
/// Everything else (browser POST with session cookie) requires a valid CSRF
/// token in the `requesttoken` header or form field.

use std::sync::OnceLock;
use regex_lite::Regex;

/// Compiled UA patterns from REQ §4.4.
/// Using `OnceLock` for lazy init without a dependency on `once_cell`.
fn ua_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // Desktop (mirall / csyncoC)
            Regex::new(r"^Mozilla/5\.0 \([A-Za-z ]+\) (?:mirall|csyncoC)/([^ ]*).*$").unwrap(),
            // Android
            Regex::new(r"^Mozilla/5\.0 \(Android\) (?:ownCloud|Nextcloud)-android/([^ ]*).*$").unwrap(),
            // iOS
            Regex::new(r"^Mozilla/5\.0 \(iOS\) (?:ownCloud|Nextcloud)-iOS/([^ ]*).*$").unwrap(),
        ]
    })
}

/// Returns `true` when the CSRF check should be **skipped** for this request.
pub fn should_skip_csrf(
    method: &str,
    ocs_api_request: bool,
    authorization_present: bool,
    user_agent: &str,
) -> bool {
    // 1. Safe HTTP methods never mutate state.
    if matches!(method, "GET" | "HEAD" | "OPTIONS") {
        return true;
    }
    // 2. OCS clients set this header to signal non-browser origin.
    if ocs_api_request {
        return true;
    }
    // 3. Requests using token/password auth are not session-based.
    if authorization_present {
        return true;
    }
    // 4. Known sync-client UAs (REQ §4.4 exact regexes).
    if is_sync_client_ua(user_agent) {
        return true;
    }
    false
}

/// Returns `true` if the User-Agent matches a known Nextcloud sync client.
///
/// Uses the exact regexes from REQ §4.4 (`IRequest.php`).
pub fn is_sync_client_ua(ua: &str) -> bool {
    ua_patterns().iter().any(|re| re.is_match(ua))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_skips_csrf() {
        assert!(should_skip_csrf("GET", false, false, "Mozilla/5.0"));
    }

    #[test]
    fn head_and_options_skip() {
        assert!(should_skip_csrf("HEAD", false, false, "curl/7.0"));
        assert!(should_skip_csrf("OPTIONS", false, false, "curl/7.0"));
    }

    #[test]
    fn ocs_apirequest_skips() {
        assert!(should_skip_csrf("POST", true, false, "Mozilla/5.0"));
    }

    #[test]
    fn authorization_header_skips() {
        assert!(should_skip_csrf("POST", false, true, "Mozilla/5.0"));
    }

    #[test]
    fn sync_client_ua_skips() {
        // Desktop
        assert!(should_skip_csrf(
            "POST", false, false,
            "Mozilla/5.0 (Linux) mirall/3.1.0 (some info)"
        ));
        // Android
        assert!(should_skip_csrf(
            "POST", false, false,
            "Mozilla/5.0 (Android) Nextcloud-android/4.0.0"
        ));
        // iOS
        assert!(should_skip_csrf(
            "POST", false, false,
            "Mozilla/5.0 (iOS) Nextcloud-iOS/4.0.0"
        ));
    }

    #[test]
    fn generic_nextcloud_ua_does_not_skip() {
        // A bare "Nextcloud" in UA without the correct pattern should NOT skip.
        assert!(!should_skip_csrf(
            "POST", false, false,
            "Nextcloud SomeOtherClient/1.0"
        ));
    }

    #[test]
    fn browser_post_requires_csrf() {
        assert!(!should_skip_csrf("POST", false, false, "Mozilla/5.0 (X11; Linux)"));
    }

    #[test]
    fn delete_without_auth_requires_csrf() {
        assert!(!should_skip_csrf("DELETE", false, false, "curl/7.0"));
    }
}
