use hmac::{Hmac, Mac};
use sha2::{Digest, Sha512};

use nc_db::pool::DbPool;

use crate::token::{CachedToken, SharedTokenCache};

type HmacSha512 = Hmac<Sha512>;

// ── Hashing ───────────────────────────────────────────────────────────────────

/// NC v2 token hash: `HMAC-SHA512(server_secret, raw_bearer_token)`.
/// Used by all installs with a `core.secret` in `oc_appconfig` (NC 20+).
pub fn hmac_hash(secret: &str, raw: &str) -> [u8; 64] {
    let mut mac = HmacSha512::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key size");
    mac.update(raw.as_bytes());
    mac.finalize().into_bytes().into()
}

/// NC v1 token hash: `SHA-512(raw_bearer_token)`.
/// Fallback for installs that pre-date the server secret (NC < 20).
pub fn sha512_hash(raw: &str) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(raw.as_bytes());
    h.finalize().into()
}

/// Compute the DB lookup hash for a raw bearer token.
///
/// Uses HMAC if `app_secret` is non-empty; falls back to plain SHA-512.
pub fn token_hash(app_secret: &str, raw: &str) -> [u8; 64] {
    if app_secret.is_empty() {
        sha512_hash(raw)
    } else {
        hmac_hash(app_secret, raw)
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

    let row: Option<(i64, String, i16, String, Option<i64>, i64)> =
        sqlx::query_as(&format!(
            "SELECT id, uid, type, scope, expires, last_activity \
             FROM {table} WHERE token = ?"
        ))
        .bind(&hash_hex)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

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
/// Spawns a `tokio` task; the caller is never delayed.
pub fn spawn_last_activity_update(token_id: i64, pool: DbPool, prefix: String) {
    tokio::spawn(async move {
        update_last_activity(token_id, &pool, &prefix).await;
    });
}

/// Blocking portion of the last_activity update (called from the spawned task).
pub async fn update_last_activity(token_id: i64, pool: &DbPool, prefix: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let table = format!("{prefix}authtoken");
    let _ = sqlx::query(&format!(
        "UPDATE {table} SET last_activity = ? WHERE id = ?"
    ))
    .bind(now)
    .bind(token_id)
    .execute(pool)
    .await;
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

    #[test]
    fn hmac_and_sha512_differ() {
        let raw = "test_token";
        let secret = "server_secret";
        let h1 = hmac_hash(secret, raw);
        let h2 = sha512_hash(raw);
        assert_ne!(h1, h2);
    }

    #[test]
    fn empty_secret_falls_back_to_sha512() {
        let raw = "test_token";
        assert_eq!(token_hash("", raw), sha512_hash(raw));
    }

    #[test]
    fn secret_uses_hmac() {
        let raw = "test_token";
        let secret = "mysecret";
        assert_eq!(token_hash(secret, raw), hmac_hash(secret, raw));
    }
}
