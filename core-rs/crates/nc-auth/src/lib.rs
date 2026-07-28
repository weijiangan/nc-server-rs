#![forbid(unsafe_code)]

pub mod basic;
pub mod bearer;
pub mod bruteforce;
pub mod csrf;
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

/// The resolved identity attached to a request after successful authentication.
///
/// Stored as an axum request extension so downstream handlers can extract it
/// with `Extension<Option<AuthInfo>>`.
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

/// How the authenticated identity was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Bearer,
    Basic,
    Session,
}
