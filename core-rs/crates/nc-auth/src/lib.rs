#![forbid(unsafe_code)]

pub mod basic;
pub mod bearer;
pub mod bruteforce;
pub mod csrf;
pub mod hasher;
pub mod session;
pub mod token;
pub mod twofa;

pub use token::{new_token_cache, SharedTokenCache};
pub use session::{
    SessionIdentity, SessionResolveResult,
    SessionCache, SharedSessionCache,
    new_session_cache, make_cache_key, cache_insert, cache_lookup, cache_evict_expired,
    SESSION_CACHE_TTL, SESSION_CACHE_EVICT_INTERVAL,
};

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use nc_db::pool::DbPool;

/// The resolved identity attached to a request after successful authentication.
///
/// Stored as an axum request extension so downstream handlers can extract it
/// with `request.extensions().get::<AuthInfo>()` (present only when the request
/// is authenticated) or the `Option<Extension<AuthInfo>>` extractor.
#[derive(Debug, Clone)]
pub struct AuthInfo {
    pub uid: String,
    pub is_admin: bool,
    pub method: AuthMethod,
    /// `oc_authtoken.id` — present for bearer and session auth; absent for basic.
    pub token_id: Option<i64>,
    /// The raw Bearer token value (before hashing), stored so the FastCGI proxy
    /// can forward it as `HTTP_X_NC_SESSION_TOKEN` to the PHP shim.
    /// `None` for Basic-password auth (plain password is never forwarded).
    pub raw_token: Option<String>,
}

/// Check whether `uid` belongs to the `admin` group.
///
/// Queries `oc_group_user WHERE gid = 'admin' AND uid = ?`.
/// Called at authenticated-request time so the admin flag is always fresh.
///
/// On DB error, returns `false` rather than failing the request — admin
/// operations will be denied rather than the entire request crashing.
pub async fn is_admin_user(uid: &str, pool: &nc_db::pool::DbPool, prefix: &str) -> bool {
    let table = format!("{prefix}group_user");
    let row: Option<(String,)> = match sqlx::query_as(&format!(
        "SELECT uid FROM {table} WHERE gid = 'admin' AND uid = $1"
    ))
    .bind(uid)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(
                uid = %uid,
                error = %e,
                "admin-group membership query failed — treating as non-admin"
            );
            return false;
        }
    };
    row.is_some()
}

// ── Phase 18.1 (round-3 Task 8): per-uid user-state cache ────────────────────

/// `(is_admin, twofa_enabled)` for one uid, resolved once per TTL window.
#[derive(Debug, Clone, Copy)]
pub struct UserState {
    pub is_admin: bool,
    pub twofa_enabled: bool,
}

/// Cached user state: `(state, cached_at)`.
type UserStateEntry = (UserState, Instant);

/// The 2FA-provider and admin-group checks run on every authenticated
/// request.  Both depend only on the uid and change rarely (2FA enablement,
/// group membership), so a short TTL is safe — the DB stays the source of
/// truth on miss, and a ≤60 s staleness is immaterial for either decision
/// (the same argument as the Phase 18.3 throttler count cache).
const USER_STATE_TTL: Duration = Duration::from_secs(60);

fn user_state_cache() -> &'static DashMap<String, UserStateEntry> {
    static CACHE: OnceLock<DashMap<String, UserStateEntry>> = OnceLock::new();
    CACHE.get_or_init(DashMap::new)
}

/// Resolve `(is_admin, twofa_enabled)` for a uid, cached for 60 s.
///
/// Combines the per-request admin-group query (`is_admin_user`) and the
/// 2FA-provider check into one cache entry, so an authenticated request pays
/// zero user-state queries instead of two.  The token-type exemption for
/// permanent app tokens (`token_type == 1`) is a pure function of the token
/// and is applied by the caller, not cached here.  On 2FA-query error the
/// error propagates (the middleware keeps its 500 semantics); the admin
/// query degrades to non-admin on error as today, never failing.
pub async fn cached_user_state(
    uid: &str,
    pool: &DbPool,
    prefix: &str,
) -> Result<UserState, sqlx::Error> {
    let cache = user_state_cache();
    if let Some(entry) = cache.get(uid) {
        if entry.1.elapsed() < USER_STATE_TTL {
            return Ok(entry.0);
        }
    }
    // Miss or stale: resolve fresh.  `twofa::requires_2fa` with token_type 0
    // returns the raw "any provider enabled" answer (no exemption applied).
    let twofa_enabled = twofa::requires_2fa(uid, 0, pool, prefix).await?;
    let is_admin = is_admin_user(uid, pool, prefix).await;
    let state = UserState { is_admin, twofa_enabled };
    cache.insert(uid.to_string(), (state, Instant::now()));
    Ok(state)
}

/// How the authenticated identity was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Bearer,
    Basic,
    Session,
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory SQLite with the two tables `cached_user_state` reads.
    async fn fresh_db() -> DbPool {
        sqlx::any::install_default_drivers();
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite");
        sqlx::query(
            "CREATE TABLE oc_twofactor_providers (
                provider_id VARCHAR(255) NOT NULL, uid VARCHAR(64) NOT NULL,
                enabled SMALLINT NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("twofactor");
        sqlx::query(
            "CREATE TABLE oc_group_user (gid VARCHAR(64) NOT NULL, uid VARCHAR(64) NOT NULL)",
        )
        .execute(&pool)
        .await
        .expect("group_user");
        pool
    }

    #[tokio::test]
    async fn cached_user_state_serves_fresh_then_cached() {
        let pool = fresh_db().await;
        let prefix = "oc_";
        sqlx::query("INSERT INTO oc_group_user (gid, uid) VALUES ('admin', 'alice')")
            .execute(&pool)
            .await
            .expect("admin");
        sqlx::query(
            "INSERT INTO oc_twofactor_providers (provider_id, uid, enabled) VALUES ('totp', 'alice', 1)",
        )
        .execute(&pool)
        .await
        .expect("2fa");

        let s = cached_user_state("alice", &pool, prefix).await.expect("resolve");
        assert!(s.is_admin, "admin membership read");
        assert!(s.twofa_enabled, "2FA provider enabled");

        // Cache serves: flip both tables in the DB and the next call must
        // return the cached values (60 s TTL).
        sqlx::query("DELETE FROM oc_group_user").execute(&pool).await.expect("del");
        sqlx::query("UPDATE oc_twofactor_providers SET enabled = 0")
            .execute(&pool)
            .await
            .expect("disable");
        let s2 = cached_user_state("alice", &pool, prefix).await.expect("cached");
        assert!(s2.is_admin, "cached admin state");
        assert!(s2.twofa_enabled, "cached 2FA state");

        // Unknown uid resolves to defaults.
        let s3 = cached_user_state("bob", &pool, prefix).await.expect("bob");
        assert!(!s3.is_admin);
        assert!(!s3.twofa_enabled);
    }
}
