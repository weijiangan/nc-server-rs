use base64::{engine::general_purpose::STANDARD, Engine};

use nc_db::pool::DbPool;

// ── Extraction ────────────────────────────────────────────────────────────────

/// Decode `Authorization: Basic {base64}` into `(username, password)`.
pub fn extract_basic(auth_header: &str) -> Option<(String, String)> {
    let encoded = auth_header.strip_prefix("Basic ")?;
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let s = std::str::from_utf8(&decoded).ok()?;
    // RFC 7617: first `:` separates userid from password.
    let colon = s.find(':')?;
    Some((s[..colon].to_string(), s[colon + 1..].to_string()))
}

// ── Verification ──────────────────────────────────────────────────────────────

/// Result of a Basic auth verification.
#[derive(Debug, Clone)]
pub struct BasicAuthResult {
    pub uid: String,
    /// `oc_authtoken.id` when the password was an app token; `None` for plain password.
    pub token_id: Option<i64>,
    /// `oc_authtoken.type` when an app token was used.
    pub token_type: Option<i16>,
}

/// Verify `(login, password)` against both `oc_users` (plain password) and
/// `oc_authtoken` (app token used as password).
///
/// Priority (REQ §4.1, §4.2, §4.3):
/// 1. Try app-token path first: hash `password` with SHA-512 (v1) or
///    HMAC-SHA512 with `app_secret` (v2), query `oc_authtoken.token` where
///    `login_name = login`. If found and not expired → success with token auth.
/// 2. Try plain-password path: `oc_users` bcrypt check.
///
/// Returns `None` on invalid credentials or DB error.
pub async fn verify_basic(
    login: &str,
    password: &str,
    pool: &DbPool,
    prefix: &str,
    app_secret: &str,
) -> Option<BasicAuthResult> {
    // ── App-token path (REQ §4.2) ─────────────────────────────────────────
    // Desktop clients send their app token as the Basic password field.
    // The token is stored hashed in oc_authtoken.token.
    if let Some(result) = try_app_token(login, password, pool, prefix, app_secret).await {
        return Some(result);
    }

    // ── Plain-password path ───────────────────────────────────────────────
    // Lookup is case-insensitive via the `uid_lower` index column.
    // bcrypt verification is off-loaded to a blocking thread pool to avoid
    // stalling the async runtime during the compute-heavy hash comparison.
    let table = format!("{prefix}users");

    let row: Option<(String, String)> = sqlx::query_as(&format!(
        "SELECT uid, password FROM {table} WHERE uid_lower = ?"
    ))
    .bind(login.to_lowercase())
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let (uid, hash) = row?;

    let password = password.to_string();
    let ok = tokio::task::spawn_blocking(move || {
        bcrypt::verify(&password, &hash).unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    if ok {
        Some(BasicAuthResult {
            uid,
            token_id: None,
            token_type: None,
        })
    } else {
        None
    }
}

/// Try to authenticate using the Basic password field as a raw app token.
///
/// Hashes the raw value the same way as bearer token lookup and queries
/// `oc_authtoken` by `token` hash + `login_name`.
async fn try_app_token(
    login: &str,
    raw_password: &str,
    pool: &DbPool,
    prefix: &str,
    app_secret: &str,
) -> Option<BasicAuthResult> {
    use crate::bearer::token_hash;

    let hash = token_hash(app_secret, raw_password);
    let hash_hex = hex::encode(hash);
    let table = format!("{prefix}authtoken");

    // REQ §4.1: if the token row has `passwordless = 1`, this token cannot
    // be used to satisfy a Basic auth challenge (the column indicates the
    // token was issued for a passwordless account — no password exists to verify).
    let row: Option<(i64, String, i16, Option<i64>)> = sqlx::query_as(&format!(
        "SELECT id, uid, type, expires \
         FROM {table} \
         WHERE token = ? AND login_name = ?"
    ))
    .bind(&hash_hex)
    .bind(login)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let (id, uid, token_type, expires) = row?;

    // Check expiry.
    if let Some(exp) = expires {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if now > exp {
            return None;
        }
    }

    Some(BasicAuthResult {
        uid,
        token_id: Some(id),
        token_type: Some(token_type),
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_extraction_standard() {
        // "alice:password" base64 = "YWxpY2U6cGFzc3dvcmQ="
        let header = "Basic YWxpY2U6cGFzc3dvcmQ=";
        let (user, pass) = extract_basic(header).unwrap();
        assert_eq!(user, "alice");
        assert_eq!(pass, "password");
    }

    #[test]
    fn basic_extraction_colon_in_password() {
        // "user:p:ass:word" — only first colon splits
        use base64::{engine::general_purpose::STANDARD, Engine};
        let encoded = STANDARD.encode("user:p:ass:word");
        let header = format!("Basic {encoded}");
        let (user, pass) = extract_basic(&header).unwrap();
        assert_eq!(user, "user");
        assert_eq!(pass, "p:ass:word");
    }

    #[test]
    fn non_basic_returns_none() {
        assert!(extract_basic("Bearer abc").is_none());
        assert!(extract_basic("Basic !!!invalid!!!").is_none());
    }

    #[test]
    fn missing_colon_returns_none() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let encoded = STANDARD.encode("nocolon");
        let header = format!("Basic {encoded}");
        assert!(extract_basic(&header).is_none());
    }
}
