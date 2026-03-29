use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

/// Maximum age of a token cache entry before it is considered stale.
pub const TOKEN_TTL: Duration = Duration::from_secs(5 * 60);

/// SHA-512 / HMAC-SHA512 digest of the raw bearer token string (64 bytes).
pub type TokenHash = [u8; 64];

/// In-process hot cache keyed on the token hash.
///
/// `RwLock::read()` is used on the hot read path, so concurrent requests never
/// block each other (only a write — which is rare — takes an exclusive lock).
pub type TokenCache = HashMap<TokenHash, CachedToken>;
pub type SharedTokenCache = Arc<RwLock<TokenCache>>;

/// Cached identity from `oc_authtoken`, held for up to [`TOKEN_TTL`].
#[derive(Debug, Clone)]
pub struct CachedToken {
    pub id: i64,
    pub uid: String,
    /// `oc_authtoken.type`: 0 = temporary (session), 1 = permanent (app token).
    pub token_type: i16,
    /// JSON-encoded scope string from `oc_authtoken.scope`.
    pub scope: String,
    /// Expiry as UNIX timestamp (seconds), or `None` = never expires.
    pub expires: Option<i64>,
    /// UNIX timestamp of last recorded activity (written back async).
    pub last_activity: i64,
    /// Wall-clock instant this entry was first cached (for TTL eviction).
    pub cached_at: Instant,
}

impl CachedToken {
    /// Returns true if `oc_authtoken.expires` is in the past.
    pub fn is_expired(&self) -> bool {
        if let Some(exp) = self.expires {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            now > exp
        } else {
            false
        }
    }

    /// Returns true if the cache entry has exceeded its TTL, regardless of
    /// the token's own expiry.
    pub fn is_cache_stale(&self) -> bool {
        self.cached_at.elapsed() >= TOKEN_TTL
    }
}

/// Initialise an empty shared token cache.
pub fn new_token_cache() -> SharedTokenCache {
    Arc::new(RwLock::new(HashMap::new()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_token(expires: Option<i64>) -> CachedToken {
        CachedToken {
            id: 1,
            uid: "alice".to_string(),
            token_type: 1,
            scope: "{}".to_string(),
            expires,
            last_activity: 0,
            cached_at: Instant::now(),
        }
    }

    #[test]
    fn non_expiring_token_is_not_expired() {
        assert!(!make_token(None).is_expired());
    }

    #[test]
    fn past_expiry_is_expired() {
        assert!(make_token(Some(1)).is_expired()); // UNIX ts 1 is 1970
    }

    #[test]
    fn future_expiry_is_not_expired() {
        let far_future = 9_999_999_999i64;
        assert!(!make_token(Some(far_future)).is_expired());
    }

    #[test]
    fn fresh_cache_entry_is_not_stale() {
        assert!(!make_token(None).is_cache_stale());
    }

    #[test]
    fn concurrent_reads_do_not_block() {
        use std::sync::Arc;
        let cache = new_token_cache();
        cache
            .write()
            .unwrap()
            .insert([0u8; 64], make_token(None));

        // Spawn 20 reader threads; if any panics (deadlock/poison) the test fails.
        let handles: Vec<_> = (0..20)
            .map(|_| {
                let c = Arc::clone(&cache);
                std::thread::spawn(move || {
                    let guard = c.read().unwrap();
                    let _ = guard.get(&[0u8; 64]).is_some();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
