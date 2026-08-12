use sha2::{Digest, Sha512};

use nc_db::pool::DbPool;

use crate::token::{CachedToken, SharedTokenCache};

// ── Hashing ───────────────────────────────────────────────────────────────────

/// PHP-compatible token hash: `SHA-512(raw_token || server_secret)`.
///
/// This matches PHP's `PublicKeyTokenProvider::hashToken()`:
/// ```php
/// return hash('sha512', $token . $secret);   // PublicKeyTokenProvider.php:412-414
/// ```
/// The two strings are simply **concatenated** before hashing — this is NOT
/// HMAC.  The output is a 64-byte digest (stored as 128-character lowercase
/// hex in `oc_authtoken.token`).
///
/// Used for all installs where `config.php` contains a `secret` value (NC 20+).
pub fn concat_hash(secret: &str, raw: &str) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(raw.as_bytes());
    h.update(secret.as_bytes());
    h.finalize().into()
}

/// PHP fallback token hash: `SHA-512(raw_token)`.
///
/// Matches PHP's `hashTokenWithEmptySecret()`:
/// ```php
/// return hash('sha512', $token);   // PublicKeyTokenProvider.php:420-421
/// ```
/// Used for pre-NC-20 installs where no server secret was set.
pub fn sha512_hash(raw: &str) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(raw.as_bytes());
    h.finalize().into()
}

/// Compute the DB lookup hash for a raw token value.
///
/// Mirrors PHP's `PublicKeyTokenProvider::hashToken()` (NC 20+) and its
/// `hashTokenWithEmptySecret()` fallback (pre-NC-20):
/// - `app_secret` non-empty → `SHA-512(raw || secret)` via [`concat_hash`]
/// - `app_secret` empty     → `SHA-512(raw)` via [`sha512_hash`]
pub fn token_hash(app_secret: &str, raw: &str) -> [u8; 64] {
    if app_secret.is_empty() {
        sha512_hash(raw)
    } else {
        concat_hash(app_secret, raw)
    }
}

// ── Extraction ────────────────────────────────────────────────────────────────

/// Extract the raw bearer value from `Authorization: Bearer {token}`.
pub fn extract_bearer(auth_header: &str) -> Option<&str> {
    auth_header.strip_prefix("Bearer ")
}

// ── Lookup ────────────────────────────────────────────────────────────────────

/// Look up a bearer token. Checks the hot cache first; falls back to DB.
///
/// Returns `None` if the token is unknown, expired, or DB lookup fails.
pub async fn lookup_bearer(
    raw_token: &str,
    pool: &DbPool,
    token_cache: &SharedTokenCache,
    app_secret: &str,
    prefix: &str,
) -> Option<CachedToken> {
    let hash = token_hash(app_secret, raw_token);

    // ── Hot-cache hit ─────────────────────────────────────────────────────
    if let Ok(guard) = token_cache.read() {
        if let Some(cached) = guard.get(&hash) {
            if !cached.is_cache_stale() && !cached.is_expired() {
                return Some(cached.clone());
            }
        }
    }

    // ── DB miss ───────────────────────────────────────────────────────────
    let hash_hex = hex::encode(hash);
    let table = format!("{prefix}authtoken");

    let row: Option<(i64, String, i16, String, Option<i64>, i64)> = match sqlx::query_as(&format!(
        "SELECT id, uid, type, scope, expires, last_activity \
                 FROM {table} WHERE token = $1"
    ))
    .bind(&hash_hex)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "bearer token lookup query failed — treating as token not found"
            );
            return None;
        }
    };

    let (id, uid, token_type, scope, expires, last_activity) = row?;

    if let Some(exp) = expires {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if now > exp {
            return None; // token expired — do not cache
        }
    }

    let cached = CachedToken {
        id,
        uid,
        token_type,
        scope,
        expires,
        last_activity,
        cached_at: std::time::Instant::now(),
    };

    if let Ok(mut guard) = token_cache.write() {
        guard.insert(hash, cached.clone());
    }

    Some(cached)
}

/// Remove a specific token from the hot cache (called on explicit revocation).
pub fn evict(raw_token: &str, app_secret: &str, token_cache: &SharedTokenCache) {
    let hash = token_hash(app_secret, raw_token);
    if let Ok(mut guard) = token_cache.write() {
        guard.remove(&hash);
    }
}

/// Fire-and-forget `oc_authtoken.last_activity` update (3.4).
///
/// Spawns a `tokio` task; the caller is never delayed.  Throttled to
/// [`ACTIVITY_UPDATE_INTERVAL`] like PHP (round-4 Task 11).
pub fn spawn_last_activity_update(token_id: i64, last_activity: i64, pool: DbPool, prefix: String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if now - last_activity < ACTIVITY_UPDATE_INTERVAL {
        return;
    }
    tokio::spawn(async move {
        update_last_activity(token_id, &pool, &prefix).await;
    });
}

/// Interval for the async `oc_authtoken.last_activity` write-back — PHP
/// parity.  PHP's `PublicKeyTokenProvider::updateTokenActivity`
/// (lib/private/Authentication/Token/PublicKeyTokenProvider.php:296) writes at
/// most once per `token_auth_activity_update` (system value, default 60 s,
/// clamped 0-300); Rust used to write on every authenticated request — a DB
/// write per request PHP would not do for a minute (a seek + journal write per
/// request on slow storage).  The check uses the cached `last_activity`
/// (≤ token-cache TTL stale), so the write rate is at most ~1/60 s per token.
const ACTIVITY_UPDATE_INTERVAL: i64 = 60;

/// Blocking portion of the last_activity update (called from the spawned task).
pub async fn update_last_activity(token_id: i64, pool: &DbPool, prefix: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let table = format!("{prefix}authtoken");
    if let Err(e) = sqlx::query(&format!(
        "UPDATE {table} SET last_activity = $1 WHERE id = $2"
    ))
    .bind(now)
    .bind(token_id)
    .execute(pool)
    .await
    {
        tracing::warn!(
            token_id,
            error = %e,
            "failed to update last_activity — session tracking may be stale"
        );
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_extraction() {
        assert_eq!(extract_bearer("Bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer("Basic xyz"), None);
        assert_eq!(extract_bearer("Bearer "), Some(""));
    }

    /// Concatenation hash and plain SHA-512 must differ when a secret is
    /// supplied (otherwise the `app_secret` would be having no effect).
    #[test]
    fn concat_and_sha512_differ() {
        let raw = "test_token";
        let secret = "server_secret";
        let h1 = concat_hash(secret, raw);
        let h2 = sha512_hash(raw);
        assert_ne!(h1, h2);
    }

    #[test]
    fn empty_secret_falls_back_to_sha512() {
        let raw = "test_token";
        assert_eq!(token_hash("", raw), sha512_hash(raw));
    }

    #[test]
    fn secret_uses_concat_hash() {
        let raw = "test_token";
        let secret = "mysecret";
        assert_eq!(token_hash(secret, raw), concat_hash(secret, raw));
    }

    /// Cross-validated against PHP's `hash('sha512', $token . $secret)`.
    ///
    /// Run via:
    /// ```bash
    /// php -r "var_dump(hash('sha512', 'test_token' . 'test_secret'));"
    /// ```
    /// Expected: `string(128) "3c8e585d...010a"`
    #[test]
    fn php_compatible_test_vector() {
        let raw = "test_token";
        let secret = "test_secret";
        let hash_bytes = concat_hash(secret, raw);
        let hash_hex = hex::encode(hash_bytes);
        assert_eq!(
            hash_hex,
            "3c8e585d127271fe9d6247bc21caa64ca3a888920db1e3ee2ebfd8678bbde825\
             e3f53bb97598278b1a64817569c5ed836a1a06aa93fc29875ea0acbf9f51010a",
        );
    }
}
