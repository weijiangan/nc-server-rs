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

/// Verify `(login, password)` against both `oc_authtoken` (app token used as
/// password) and `oc_users` (plain password).
///
/// Priority (REQ §4.1, §4.2, §4.3):
/// 1. Try app-token path first: hash `password` with SHA-512 (v1 fallback) or
///    SHA-512(`password` ‖ `app_secret`) (v2), query `oc_authtoken.token` by
///    hash only (matching PHP's `PublicKeyTokenMapper::getToken()`).  After
///    fetching the row, validate `login` against the stored `login_name`
///    case-insensitively (matching PHP's `validateTokenLoginName()`).  If found,
///    not expired, and login matches → success with token auth.
/// 2. Try plain-password path: `oc_users.password` via the PHP-compatible
///    [`crate::hasher`] (argon2id / argon2i / bcrypt / legacy SHA-1).
///
/// `legacy_salt` is the `passwordsalt` system-config value, used only by the
/// hasher's legacy path.
///
/// Returns `None` on invalid credentials or DB error.
pub async fn verify_basic(
    login: &str,
    password: &str,
    pool: &DbPool,
    prefix: &str,
    app_secret: &str,
    legacy_salt: &str,
) -> Option<BasicAuthResult> {
    // ── App-token path (REQ §4.2) ─────────────────────────────────────────
    // Desktop clients send their app token as the Basic password field.
    // The token is stored hashed in oc_authtoken.token.
    if let Some(result) = try_app_token(login, password, pool, prefix, app_secret).await {
        return Some(result);
    }

    // ── Plain-password path ───────────────────────────────────────────────
    // Lookup is case-insensitive via the `uid_lower` index column.
    // Hash verification (argon2id by default) is CPU-heavy, so it is off-loaded
    // to a blocking thread pool to avoid stalling the async runtime.
    let table = format!("{prefix}users");

    let sql = format!("SELECT uid, password FROM {table} WHERE uid_lower = $1");
    let login_lower = login.to_lowercase();
    let fetched: Result<Option<(String, String)>, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query_as::<sqlx::Postgres, (String, String)>(&sql)
            .bind(&login_lower)
            .fetch_optional(p)
            .await,
        DbPool::Sqlite(p) => sqlx::query_as::<sqlx::Sqlite, (String, String)>(&sql)
            .bind(&login_lower)
            .fetch_optional(p)
            .await,
    };
    let row: Option<(String, String)> = match fetched {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "plain-password user lookup query failed — treating as wrong password"
            );
            return None;
        }
    };

    let (uid, hash) = row?;

    let password = password.to_string();
    let legacy_salt = legacy_salt.to_string();
    let ok = tokio::task::spawn_blocking(move || {
        crate::hasher::verify_password(&password, &hash, &legacy_salt)
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
/// `oc_authtoken` by `token` hash only (matching PHP).  After fetching the
/// row, validates the provided `login` against the stored `login_name`
/// case-insensitively (matching PHP's `validateTokenLoginName()`).
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

    // REQ §4.1: query by token hash only (matching PHP's `PublicKeyTokenMapper::getToken()`
    // which filters `WHERE token = $hash AND version = $version` — no `login_name` in the
    // WHERE clause).  The `login_name` validation happens below, matching PHP's
    // `validateTokenLoginName()` case-insensitive comparison.
    let sql = format!(
        "SELECT id, uid, type, expires, login_name \
         FROM {table} \
         WHERE token = $1"
    );
    let fetched: Result<Option<(i64, String, i16, Option<i64>, String)>, sqlx::Error> = match pool {
        DbPool::Pg(p) => sqlx::query_as::<sqlx::Postgres, (i64, String, i16, Option<i64>, String)>(
            &sql,
        )
        .bind(&hash_hex)
        .fetch_optional(p)
        .await,
        DbPool::Sqlite(p) => {
            sqlx::query_as::<sqlx::Sqlite, (i64, String, i16, Option<i64>, String)>(&sql)
                .bind(&hash_hex)
                .fetch_optional(p)
                .await
        }
    };
    let row: Option<(i64, String, i16, Option<i64>, String)> = match fetched {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "app-token lookup query failed — treating as wrong password"
            );
            return None;
        }
    };

    let (id, uid, token_type, expires, db_login_name) = row?;

    // REQ §4.1: validate login name matches the token's stored `login_name`
    // (case-insensitive, matching PHP's `mb_strtolower` in `validateTokenLoginName()`).
    // PHP `Session.php:793-809`: `mb_strtolower($token->getLoginName()) !== mb_strtolower($loginName)`.
    if db_login_name.to_lowercase() != login.to_lowercase() {
        return None;
    }

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
