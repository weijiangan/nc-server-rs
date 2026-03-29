#![forbid(unsafe_code)]

pub mod basic;
pub mod bearer;
pub mod bruteforce;
pub mod csrf;
pub mod session;
pub mod token;
pub mod twofa;

pub use token::{new_token_cache, SharedTokenCache};

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
pub async fn is_admin_user(uid: &str, pool: &nc_db::pool::DbPool, prefix: &str) -> bool {
    let table = format!("{prefix}group_user");
    let row: Option<(String,)> =
        sqlx::query_as(&format!("SELECT uid FROM {table} WHERE gid = 'admin' AND uid = ?"))
            .bind(uid)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    row.is_some()
}

/// How the authenticated identity was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Bearer,
    Basic,
    Session,
}
