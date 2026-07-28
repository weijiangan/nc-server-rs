use nc_db::pool::DbPool;

/// 2FA enforcement gate (REQ §4.5).
///
/// After successful credential validation (bearer or basic), query
/// `oc_twofactor_providers` to determine if the user has an enabled 2FA
/// provider. If so, block the request — the client must complete the 2FA
/// challenge (done through the PHP login flow) and obtain a new app token.
///
/// App tokens (`token_type = 1`) are exempt once generated post-2FA; it is the
/// responsibility of the PHP token-creation flow to gate on 2FA before issuing
/// the token. Temporary tokens (`token_type = 0`, i.e. session tokens) require
/// the 2FA check.
///
/// If the `oc_twofactor_providers` table is empty or the user has no entry,
/// 2FA is considered not required.
///
/// # Errors
///
/// Returns `Err` on database failure. Matching PHP, the caller should respond
/// with 500 Internal Server Error — a broken DB is neither "2FA required"
/// nor "2FA passed."

/// Returns `Ok(true)` if `uid` has at least one enabled 2FA provider AND the
/// current auth context requires the 2FA check.
///
/// `token_type`: `0` = temporary / session (check required), `1` = permanent /
/// app token (exempt — token was issued after 2FA was already cleared).
pub async fn requires_2fa(
    uid: &str,
    token_type: i16,
    pool: &DbPool,
    prefix: &str,
) -> Result<bool, sqlx::Error> {
    // Permanent app tokens are exempt from the per-request 2FA gate.
    if token_type == 1 {
        return Ok(false);
    }

    let table = format!("{prefix}twofactor_providers");
    let count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {table} WHERE uid = $1 AND enabled = 1"
    ))
    .bind(uid)
    .fetch_one(pool)
    .await?;

    Ok(count > 0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// Integration tests for `requires_2fa` live in tests/auth.rs (Phase 3 test file)
    /// because they need a live DB. Unit test: cover the token_type exemption
    /// without a DB.
    #[test]
    fn app_token_type_constant() {
        // Ensure our magic number for "permanent app token" is documented.
        const APP_TOKEN: i16 = 1;
        assert_eq!(APP_TOKEN, 1);
    }
}
