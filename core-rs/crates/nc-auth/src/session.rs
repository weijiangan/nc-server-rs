/// Session-cookie based authentication.
///
/// ## Strict-cookie (SameSite) guard
/// The trigger condition (PHP source: `Request.php:466-474`) is the presence of
/// the **PHP session cookie** (named after `config.php`'s `instanceid` value,
/// e.g. `oc1a2b3c4d5e`) **or** `nc_token`.  Note: `nc_session_id` is a
/// separate remember-me cookie and is **not** a trigger.
///
/// When triggered, `nc_sameSiteCookielax=true` and `nc_sameSiteCookiestrict=true`
/// (with `__Host-` prefix on HTTPS) must both be present.  On failure PHP
/// returns HTTP 412 Precondition Failed (base.php strict cookie check).
///
/// ## Session identity resolution
/// Actual PHP session identity is resolved via the FastCGI `__session_resolve`
/// endpoint (Phase 7.9.3).  The `SessionIdentity` and `SessionResolveResult`
/// types defined here carry the result.
///
/// ## Session identity cache (§7.9.5)
/// `SessionCache` is a `DashMap<[u8; 32], (SessionIdentity, Instant)>` keyed
/// on `SHA-256(php_session_cookie_value)`.  It is held in `AppState` and
/// checked before calling `resolve_session()` on every PHP-session-only
/// browser request.
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default TTL for positive entries in the session identity cache.
///
/// PHP session logout/invalidation takes effect within this window.  This is
/// the remember-me **revocation knob** (Wave 2.1): lowering it trades one
/// FastCGI round-trip per browser request for faster logout propagation
/// (10-15 s is the recommended operating range when revocation latency
/// matters); the per-request effective value comes from `session_cache_ttl`
/// in `config.php` when set.
pub const SESSION_CACHE_TTL: Duration = Duration::from_secs(60);

/// TTL for negative entries — a failed `__session_resolve` (junk cookie,
/// expired session, unauthenticated) is cached for this long so an
/// attacker's request burst hits memory, not PHP-FPM (F3, Wave 2.1).
/// Long enough to absorb a burst, short enough that a just-completed PHP
/// login is barely delayed.
pub const SESSION_NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(5);

/// How often expired entries are proactively evicted.
pub const SESSION_CACHE_EVICT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Key type: `SHA-256` of the raw PHP session cookie value.
pub type SessionCacheKey = [u8; 32];

/// One cache entry: a resolved identity (positive) or a failed resolution
/// (negative).  Negative entries expire on [`SESSION_NEGATIVE_CACHE_TTL`],
/// positive ones on the positive TTL — both are evicted together.
#[derive(Debug, Clone)]
pub enum SessionCacheEntry {
    Positive(SessionIdentity, Instant),
    Negative(Instant),
}

/// Shared session-identity cache (§7.9.5).
///
/// `DashMap` gives lock-free concurrent reads on the hot path.
pub type SessionCache = dashmap::DashMap<SessionCacheKey, SessionCacheEntry>;
pub type SharedSessionCache = Arc<SessionCache>;

/// Compute the `DashMap` cache key for a PHP session cookie value.
///
/// Key = `SHA-256(raw_cookie_value_bytes)`.  Using a hash avoids storing the
/// raw session ID in memory (defence-in-depth) and gives a compact fixed-size
/// key.
pub fn make_cache_key(cookie_value: &str) -> SessionCacheKey {
    let mut h = Sha256::new();
    h.update(cookie_value.as_bytes());
    h.finalize().into()
}

/// Allocate a new empty shared session cache.
pub fn new_session_cache() -> SharedSessionCache {
    Arc::new(SessionCache::new())
}

/// Insert or replace a resolved identity in the cache.
///
/// The `inserted_at` timestamp is set to `Instant::now()`.
pub fn cache_insert(cache: &SessionCache, key: SessionCacheKey, identity: SessionIdentity) {
    cache.insert(key, SessionCacheEntry::Positive(identity, Instant::now()));
}

/// Record a failed resolution under `key` (F3, Wave 2.1).
///
/// A negative entry makes [`cache_lookup`] report `CacheLookup::Negative`
/// until it expires after [`SESSION_NEGATIVE_CACHE_TTL`] — the caller treats
/// that exactly like a fresh failed `__session_resolve` (anonymous) but
/// without touching PHP-FPM.
pub fn cache_insert_negative(cache: &SessionCache, key: SessionCacheKey) {
    cache.insert(key, SessionCacheEntry::Negative(Instant::now()));
}

/// Outcome of [`cache_lookup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup {
    /// Fresh positive entry — the cached identity.
    Positive(SessionIdentity),
    /// Fresh negative entry — the last resolution for this cookie failed
    /// within the negative TTL; treat as anonymous without re-resolving.
    Negative,
    /// No entry, or the entry has exceeded its TTL — resolve again.
    Miss,
}

/// Look up a cached entry.
///
/// `positive_ttl` is the effective positive-entry TTL for this request (the
/// `session_cache_ttl` config value, default [`SESSION_CACHE_TTL`]) — the
/// revocation knob.  Negative entries always use
/// [`SESSION_NEGATIVE_CACHE_TTL`].
pub fn cache_lookup(
    cache: &SessionCache,
    key: &SessionCacheKey,
    positive_ttl: Duration,
) -> CacheLookup {
    let Some(entry) = cache.get(key) else {
        return CacheLookup::Miss;
    };
    match *entry.value() {
        SessionCacheEntry::Positive(ref identity, inserted_at) => {
            if inserted_at.elapsed() < positive_ttl {
                CacheLookup::Positive(identity.clone())
            } else {
                CacheLookup::Miss
            }
        }
        SessionCacheEntry::Negative(inserted_at) => {
            if inserted_at.elapsed() < SESSION_NEGATIVE_CACHE_TTL {
                CacheLookup::Negative
            } else {
                CacheLookup::Miss
            }
        }
    }
}

/// Remove all entries older than their own TTL (positive by
/// [`SESSION_CACHE_TTL`], negative by [`SESSION_NEGATIVE_CACHE_TTL`]).
///
/// Called periodically (every [`SESSION_CACHE_EVICT_INTERVAL`]) to prevent
/// unbounded map growth.  TTL is the only eviction mechanism — there is no
/// explicit logout invalidation.  A `session_cache_ttl` config value LOWER
/// than the default only sharpens lookup-time expiry; the periodic eviction
/// with the default TTL still bounds the map.
pub fn cache_evict_expired(cache: &SessionCache) {
    cache.retain(|_, entry| match entry {
        SessionCacheEntry::Positive(_, inserted_at) => inserted_at.elapsed() < SESSION_CACHE_TTL,
        SessionCacheEntry::Negative(inserted_at) => {
            inserted_at.elapsed() < SESSION_NEGATIVE_CACHE_TTL
        }
    });
}

/// Result of the strict-cookie guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieCheck {
    /// Neither the PHP session cookie (`{instanceid}`) nor `nc_token` is
    /// present — nothing to validate.
    NoSessionCookies,
    /// Session cookie present and SameSite guard cookies also present.
    Valid,
    /// Session cookie present but SameSite guard cookies missing.
    /// PHP returns HTTP 412 Precondition Failed for this condition.
    StrictCheckFailed,
}

/// Run the SameSite guard (strict cookie check).
///
/// `instanceid` is the PHP session cookie name (from `config.php`'s
/// `instanceid` key, e.g. `"oc1a2b3c4d5e"`).
/// `cookies` is the raw value of the `Cookie:` header.
///
/// The trigger is the presence of the `{instanceid}` cookie **or** `nc_token`.
/// An empty `instanceid` falls back to checking only `nc_token`.
pub fn check_samesite_cookies(cookies: &str, instanceid: &str, is_https: bool) -> CookieCheck {
    // Trigger: PHP session cookie ({instanceid}) OR nc_token present.
    // nc_session_id is NOT the trigger — it is a separate remember-me cookie.
    let has_session = (!instanceid.is_empty() && cookie_value(cookies, instanceid).is_some())
        || cookie_value(cookies, "nc_token").is_some();

    if !has_session {
        return CookieCheck::NoSessionCookies;
    }

    // On HTTPS, the SameSite cookies have the __Host- prefix.
    let (lax_key, strict_key) = if is_https {
        (
            "__Host-nc_sameSiteCookielax",
            "__Host-nc_sameSiteCookiestrict",
        )
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

/// Extract the PHP session cookie value.
///
/// Checks the `{instanceid}` cookie first (the actual PHP session cookie),
/// then falls back to `nc_token` (remember-me token).  An empty `instanceid`
/// skips the first check.  Returns `None` when neither is present.
pub fn session_cookie_value<'a>(instanceid: &str, cookies: &'a str) -> Option<&'a str> {
    if !instanceid.is_empty() {
        cookie_value(cookies, instanceid).or_else(|| cookie_value(cookies, "nc_token"))
    } else {
        cookie_value(cookies, "nc_token")
    }
}

/// Whether the remember-me login cookies are all present.
///
/// PHP's `OC::handleLogin()` (base.php:1225-1249) re-logs-in a request with
/// the remember-me path only when ALL THREE of `nc_username`, `nc_token`,
/// and `nc_session_id` are set (base.php:1239-1242).  The Rust auth
/// middleware uses this to decide whether a failed `__session_resolve` is
/// definitive: a read-only resolve (`login = false`, proxied path) that
/// fails while remember-me is present must NOT be negative-cached, because
/// the real PHP request that follows will attempt the remember-me login and
/// may succeed — caching the failure masks that re-login for the negative
/// TTL and 401s the next Rust-native DAV request (live incident 2026-09-02).
pub fn has_remember_me_cookies(cookies: &str) -> bool {
    cookie_value(cookies, "nc_username").is_some()
        && cookie_value(cookies, "nc_token").is_some()
        && cookie_value(cookies, "nc_session_id").is_some()
}

/// Resolved identity from a PHP session.
///
/// Populated by the FastCGI `__session_resolve` endpoint (Phase 7.9.3) after
/// `OC::handleLogin()` has run the full PHP auth chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    /// UID of the authenticated user.
    pub uid: String,
    /// UID stored in `$_SESSION['AUTHENTICATED_TO_DAV_BACKEND']` by
    /// `apps/dav/lib/Connector/Sabre/Auth.php` on the first DAV request in
    /// this session.  `None` when absent.
    pub dav_authenticated_uid: Option<String>,
}

/// Result returned by `nc_fastcgi::resolve_session()`.
#[derive(Debug, Clone)]
pub struct SessionResolveResult {
    /// The resolved user identity.
    pub identity: SessionIdentity,
    /// `Set-Cookie` headers from the PHP shim response (e.g. a rotated
    /// `nc_token` from the remember-me path).  Must be forwarded to the
    /// client when non-empty so the browser receives updated cookies.
    pub set_cookies: Vec<String>,
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

    // ── check_samesite_cookies ────────────────────────────────────────────

    #[test]
    fn no_session_cookies() {
        assert_eq!(
            check_samesite_cookies("foo=bar", "oc1abc", false),
            CookieCheck::NoSessionCookies
        );
    }

    /// The trigger cookie is the `{instanceid}` cookie, not `nc_session_id`.
    /// An `nc_session_id` cookie present alone must NOT trigger the check.
    #[test]
    fn samesite_trigger_uses_instanceid_not_nc_session_id() {
        // nc_session_id alone should NOT trigger (it is a remember-me cookie)
        let cookies = "nc_session_id=sid; nc_sameSiteCookielax=true; nc_sameSiteCookiestrict=true";
        assert_eq!(
            check_samesite_cookies(cookies, "oc1abc", false),
            CookieCheck::NoSessionCookies,
            "nc_session_id must not trigger the strict cookie check"
        );

        // The instanceid cookie SHOULD trigger
        let cookies = "oc1abc=sid; nc_sameSiteCookielax=true; nc_sameSiteCookiestrict=true";
        assert_eq!(
            check_samesite_cookies(cookies, "oc1abc", false),
            CookieCheck::Valid,
            "instanceid cookie must trigger the strict cookie check"
        );
    }

    /// `nc_token` alone triggers the check regardless of instanceid cookie absence.
    #[test]
    fn samesite_trigger_nc_token_still_works() {
        let cookies = "nc_token=tok; nc_sameSiteCookielax=true; nc_sameSiteCookiestrict=true";
        assert_eq!(
            check_samesite_cookies(cookies, "oc1abc", false),
            CookieCheck::Valid
        );
    }

    /// When the trigger cookie is present but guard cookies are absent, the
    /// result must be `StrictCheckFailed` (PHP returns 412 for this).
    #[test]
    fn samesite_failure_returns_strict_check_failed() {
        // Instanceid present, guard cookies absent
        let cookies = "oc1abc=sid; nc_sameSiteCookielax=true";
        assert_eq!(
            check_samesite_cookies(cookies, "oc1abc", false),
            CookieCheck::StrictCheckFailed
        );

        // nc_token present, guard cookies absent
        let cookies = "nc_token=tok";
        assert_eq!(
            check_samesite_cookies(cookies, "oc1abc", false),
            CookieCheck::StrictCheckFailed
        );
    }

    #[test]
    fn strict_check_passes_http() {
        let cookies = "oc1abc=sid; nc_sameSiteCookielax=true; nc_sameSiteCookiestrict=true";
        assert_eq!(
            check_samesite_cookies(cookies, "oc1abc", false),
            CookieCheck::Valid
        );
    }

    #[test]
    fn strict_check_fails_missing_guard() {
        let cookies = "oc1abc=sid; nc_sameSiteCookielax=true";
        assert_eq!(
            check_samesite_cookies(cookies, "oc1abc", false),
            CookieCheck::StrictCheckFailed
        );
    }

    #[test]
    fn strict_check_passes_https_with_host_prefix() {
        let cookies =
            "oc1abc=sid; __Host-nc_sameSiteCookielax=true; __Host-nc_sameSiteCookiestrict=true";
        assert_eq!(
            check_samesite_cookies(cookies, "oc1abc", true),
            CookieCheck::Valid
        );
    }

    // ── session_cookie_value ──────────────────────────────────────────────

    /// Returns the `{instanceid}` cookie value when present.
    #[test]
    fn session_cookie_value_finds_instanceid_cookie() {
        let cookies = "oc1abc=sid123; nc_token=tok";
        assert_eq!(session_cookie_value("oc1abc", cookies), Some("sid123"));
    }

    /// Falls back to `nc_token` when the instanceid cookie is absent.
    #[test]
    fn session_cookie_value_falls_back_to_nc_token() {
        let cookies = "nc_token=tok123";
        assert_eq!(session_cookie_value("oc1abc", cookies), Some("tok123"));
    }

    /// Returns `None` when neither the instanceid cookie nor `nc_token` is present.
    #[test]
    fn session_cookie_value_returns_none_when_both_absent() {
        let cookies = "nc_session_id=sid; foo=bar";
        assert_eq!(session_cookie_value("oc1abc", cookies), None);
    }

    // ── has_remember_me_cookies ────────────────────────────────────────────

    /// All three remember-me cookies present → true.
    #[test]
    fn remember_me_present_with_all_three() {
        let cookies =
            "nc_username=alice; nc_token=tok123; nc_session_id=sid123; nc_sameSiteCookielax=true";
        assert!(has_remember_me_cookies(cookies));
    }

    /// Missing any one of the three → false (PHP's remember-me branch needs
    /// all three; base.php:1241-1245).
    #[test]
    fn remember_me_missing_username() {
        let cookies = "nc_token=tok123; nc_session_id=sid123";
        assert!(!has_remember_me_cookies(cookies));
    }

    #[test]
    fn remember_me_missing_token() {
        let cookies = "nc_username=alice; nc_session_id=sid123";
        assert!(!has_remember_me_cookies(cookies));
    }

    #[test]
    fn remember_me_missing_session_id() {
        let cookies = "nc_username=alice; nc_token=tok123";
        assert!(!has_remember_me_cookies(cookies));
    }

    /// No remember-me cookies at all → false.
    #[test]
    fn remember_me_absent() {
        assert!(!has_remember_me_cookies("oc1abc=somesessionid"));
    }

    // ── cookie_value (internal helper) ────────────────────────────────────

    #[test]
    fn cookie_value_extraction() {
        let cookies = "a=1; b=hello; c=";
        assert_eq!(cookie_value(cookies, "a"), Some("1"));
        assert_eq!(cookie_value(cookies, "b"), Some("hello"));
        assert_eq!(cookie_value(cookies, "c"), Some(""));
        assert_eq!(cookie_value(cookies, "d"), None);
    }

    // ── session cache (§7.9.5) ────────────────────────────────────────────

    fn sample_identity() -> SessionIdentity {
        SessionIdentity {
            uid: "alice".to_string(),
            dav_authenticated_uid: None,
        }
    }

    /// `make_cache_key` is deterministic: same input → same output.
    /// Different inputs → different outputs.
    #[test]
    fn session_cache_key_is_sha256_of_cookie_value() {
        let k1 = make_cache_key("sid123");
        let k2 = make_cache_key("sid123");
        let k3 = make_cache_key("sidXXX");

        assert_eq!(k1, k2, "same cookie value must produce the same key");
        assert_ne!(
            k1, k3,
            "different cookie values must produce different keys"
        );

        // Verify it is actually SHA-256: PHP session IDs are opaque strings;
        // we just check the length (32 bytes = 256 bits).
        assert_eq!(k1.len(), 32);

        // Cross-check against the known SHA-256 of b"sid123":
        // sha256("sid123") = "af2bdbe1aa9b6ec1e2ade1d694f41fc71a831d0268e9891562113d8a62add1bf"
        let expected = {
            let mut h = sha2::Sha256::new();
            sha2::Digest::update(&mut h, b"sid123");
            let out: [u8; 32] = sha2::Digest::finalize(h).into();
            out
        };
        assert_eq!(k1, expected);
    }

    /// A freshly inserted entry is returned within its TTL.
    #[test]
    fn session_cache_hit_within_ttl() {
        let cache = SessionCache::new();
        let key = make_cache_key("sessionid-abc");
        cache_insert(&cache, key, sample_identity());

        let result = cache_lookup(&cache, &key, SESSION_CACHE_TTL);
        assert_eq!(result, CacheLookup::Positive(sample_identity()));
    }

    /// A cache miss returns `Miss`.
    #[test]
    fn session_cache_miss_for_unknown_key() {
        let cache = SessionCache::new();
        let key = make_cache_key("nonexistent");
        assert_eq!(
            cache_lookup(&cache, &key, SESSION_CACHE_TTL),
            CacheLookup::Miss
        );
    }

    /// An entry inserted with a deliberately aged timestamp is treated as
    /// expired (TTL exceeded).
    ///
    /// We cannot travel the `Instant` clock backwards, so we use a backdated
    /// raw insertion to simulate an aged entry.
    #[test]
    fn session_cache_miss_after_ttl() {
        let cache = SessionCache::new();
        let key = make_cache_key("old-session");
        // Insert with a timestamp 61 s in the past by using an Instant from
        // Instant::now() minus SESSION_CACHE_TTL - 1 s extra.
        let old_time = Instant::now()
            .checked_sub(SESSION_CACHE_TTL + Duration::from_secs(1))
            .expect("instant subtraction should not underflow on test systems");
        cache.insert(
            key,
            SessionCacheEntry::Positive(sample_identity(), old_time),
        );

        // Should be considered expired → Miss.
        assert_eq!(
            cache_lookup(&cache, &key, SESSION_CACHE_TTL),
            CacheLookup::Miss
        );
    }

    /// The `session_cache_ttl` revocation knob: a positive entry that the
    /// default TTL would still serve is already `Miss` under a shorter TTL.
    #[test]
    fn session_cache_positive_ttl_is_a_knob() {
        let cache = SessionCache::new();
        let key = make_cache_key("knob-session");
        let old_time = Instant::now()
            .checked_sub(Duration::from_secs(30))
            .expect("instant subtraction should not underflow on test systems");
        cache.insert(
            key,
            SessionCacheEntry::Positive(sample_identity(), old_time),
        );

        assert_eq!(
            cache_lookup(&cache, &key, Duration::from_secs(60)),
            CacheLookup::Positive(sample_identity()),
            "30 s old entry is fresh under the default 60 s TTL"
        );
        assert_eq!(
            cache_lookup(&cache, &key, Duration::from_secs(10)),
            CacheLookup::Miss,
            "same entry is expired under a 10 s revocation knob"
        );
    }

    /// `cache_evict_expired` removes entries older than TTL and retains fresh ones.
    #[test]
    fn session_cache_eviction_removes_expired() {
        let cache = SessionCache::new();

        let fresh_key = make_cache_key("fresh-session");
        let stale_key = make_cache_key("stale-session");

        // Fresh entry: inserted now.
        cache_insert(&cache, fresh_key, sample_identity());

        // Stale entry: backdated beyond TTL.
        let old_time = Instant::now()
            .checked_sub(SESSION_CACHE_TTL + Duration::from_secs(1))
            .expect("instant subtraction should not underflow on test systems");
        cache.insert(
            stale_key,
            SessionCacheEntry::Positive(sample_identity(), old_time),
        );

        assert_eq!(cache.len(), 2);

        cache_evict_expired(&cache);

        assert_eq!(cache.len(), 1, "stale entry should have been evicted");
        assert!(
            cache.get(&fresh_key).is_some(),
            "fresh entry must be retained"
        );
        assert!(cache.get(&stale_key).is_none(), "stale entry must be gone");
    }

    // ── Negative caching (F3, Wave 2.1) ─────────────────────────────────────

    /// A negative entry is served within its TTL — repeated junk cookies hit
    /// memory, not PHP-FPM.
    #[test]
    fn negative_result_cached_within_ttl() {
        let cache = SessionCache::new();
        let key = make_cache_key("junk-cookie");
        cache_insert_negative(&cache, key);

        let result = cache_lookup(&cache, &key, SESSION_CACHE_TTL);
        assert_eq!(result, CacheLookup::Negative);
    }

    /// A negative entry expires after [`SESSION_NEGATIVE_CACHE_TTL`] — a
    /// just-completed PHP login is barely delayed (the next request resolves
    /// again).
    #[test]
    fn negative_cache_expires_after_ttl() {
        let cache = SessionCache::new();
        let key = make_cache_key("expiring-junk");
        let old_time = Instant::now()
            .checked_sub(SESSION_NEGATIVE_CACHE_TTL + Duration::from_secs(1))
            .expect("instant subtraction should not underflow on test systems");
        cache.insert(key, SessionCacheEntry::Negative(old_time));

        assert_eq!(
            cache_lookup(&cache, &key, SESSION_CACHE_TTL),
            CacheLookup::Miss,
            "negative entry past its TTL must be re-resolvable"
        );
    }

    /// Positive and negative entries share the same map and eviction — the
    /// periodic sweep drops both kinds past their own TTLs and keeps both
    /// kinds when fresh.
    #[test]
    fn positive_and_negative_entries_share_eviction() {
        let cache = SessionCache::new();

        let fresh_pos = make_cache_key("fresh-pos");
        let fresh_neg = make_cache_key("fresh-neg");
        let stale_neg = make_cache_key("stale-neg");

        cache_insert(&cache, fresh_pos, sample_identity());
        cache_insert_negative(&cache, fresh_neg);
        let stale_time = Instant::now()
            .checked_sub(SESSION_NEGATIVE_CACHE_TTL + Duration::from_secs(1))
            .expect("instant subtraction should not underflow on test systems");
        cache.insert(stale_neg, SessionCacheEntry::Negative(stale_time));

        assert_eq!(cache.len(), 3);
        cache_evict_expired(&cache);
        assert_eq!(
            cache.len(),
            2,
            "stale negative evicted, fresh of both kinds kept"
        );
        assert!(cache.get(&fresh_pos).is_some());
        assert!(cache.get(&fresh_neg).is_some());
        assert!(cache.get(&stale_neg).is_none());
    }
}
