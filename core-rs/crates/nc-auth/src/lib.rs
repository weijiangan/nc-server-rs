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

/// Per-uid user state, resolved once per TTL window.
#[derive(Debug, Clone)]
pub struct UserState {
    pub is_admin: bool,
    pub twofa_enabled: bool,
    /// `shareapi_exclude_groups` applies to this uid — input to the sharing
    /// mask in PROPFIND (round-4 Task 12).
    pub sharing_disabled: bool,
    /// `oc_users.displayname` → `oc_accounts.data` → UID (REQ §6.5 / §4.8).
    pub display_name: String,
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

/// Resolve the per-uid user state, cached for 60 s.
///
/// Combines the admin-group query (`is_admin_user`), the 2FA-provider check,
/// the sharing-mask config (`sharing_disabled_for_user`), and the display
/// name into one cache entry — an authenticated request pays zero user-state
/// queries instead of up to five (round-3 Task 8, round-4 Task 12).  The
/// token-type exemption for permanent app tokens (`token_type == 1`) is a
/// pure function of the token and is applied by the caller, not cached here.
/// On 2FA-query error the error propagates (the middleware keeps its 500
/// semantics); the other three queries degrade to defaults on error, never
/// failing.
pub async fn cached_user_state(
    uid: &str,
    pool: &DbPool,
    prefix: &str,
) -> Result<UserState, sqlx::Error> {
    let cache = user_state_cache();
    if let Some(entry) = cache.get(uid) {
        if entry.1.elapsed() < USER_STATE_TTL {
            return Ok(entry.0.clone());
        }
    }
    // Miss or stale: resolve fresh.  `twofa::requires_2fa` with token_type 0
    // returns the raw "any provider enabled" answer (no exemption applied).
    let twofa_enabled = twofa::requires_2fa(uid, 0, pool, prefix).await?;
    let is_admin = is_admin_user(uid, pool, prefix).await;
    let sharing_disabled = sharing_disabled_for_user(pool, prefix, uid).await;
    let display_name = lookup_user_display_name(pool, prefix, uid).await;
    let state = UserState { is_admin, twofa_enabled, sharing_disabled, display_name };
    cache.insert(uid.to_string(), (state.clone(), Instant::now()));
    Ok(state)
}

/// `shareapi_exclude_groups` applies to `uid` — ported from nc-dav's row
/// helper so the per-uid TTL cache covers it.  Reads two `oc_appconfig`
/// keys (`shareapi_exclude_groups`, `shareapi_exclude_groups_list`) plus the
/// user's `oc_group_user` memberships; "no"/empty means sharing is enabled
/// for everyone (PHP `SetupManager` sharing_mask semantics).
async fn sharing_disabled_for_user(pool: &DbPool, prefix: &str, uid: &str) -> bool {
    let key = "shareapi_exclude_groups";
    let sql = format!(
        "SELECT configvalue FROM {prefix}appconfig WHERE appid = 'core' AND configkey = $1"
    );
    let exclude_groups: Option<String> = sqlx::query_scalar(&sql)
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

    match exclude_groups.as_deref() {
        None | Some("no") | Some("") => {
            // Sharing is not restricted by group — enabled for everyone.
            false
        }
        Some(mode @ ("yes" | "allow")) => {
            // Read the group list.
            let list_sql = format!(
                "SELECT configvalue FROM {prefix}appconfig WHERE appid = 'core' AND configkey = 'shareapi_exclude_groups_list'"
            );
            let list_val: Option<String> = sqlx::query_scalar(&list_sql)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();

            let excluded_groups: Vec<String> = match list_val.as_deref() {
                Some(s) if !s.is_empty() => {
                    // PHP tries json_decode first, then explodes on comma.
                    serde_json::from_str::<Vec<String>>(s)
                        .unwrap_or_else(|_| s.split(',').map(|g| g.trim().to_string()).collect())
                }
                _ => vec![],
            };

            if excluded_groups.is_empty() {
                return false;
            }

            // Query the user's group memberships.
            let groups_sql = format!("SELECT gid FROM {prefix}group_user WHERE uid = $1");
            let user_groups: Vec<String> = match sqlx::query_scalar::<_, String>(&groups_sql)
                .bind(uid)
                .fetch_all(pool)
                .await
            {
                Ok(g) => g,
                Err(_) => return false,
            };

            if mode == "allow" {
                // Allowlist: sharing allowed only if user is in at least one
                // allowed group.  If user is in no groups at all, they can't
                // be in an allowed group → disabled.
                let in_allowed = user_groups.iter().any(|g| excluded_groups.contains(g));
                !in_allowed
            } else {
                // Exclude mode: sharing disabled only if ALL user groups are
                // excluded.  PHP: if (!empty($usersGroups)) guards the diff;
                // empty groups → falls through to return false (sharing NOT
                // disabled).
                if user_groups.is_empty() {
                    false
                } else {
                    user_groups.iter().all(|g| excluded_groups.contains(g))
                }
            }
        }
        Some(other) => {
            tracing::warn!(
                %other,
                "unexpected value for shareapi_exclude_groups; treating as sharing enabled"
            );
            false
        }
    }
}

/// Display name for `uid`: `oc_users.displayname` → `oc_accounts.data` →
/// UID — ported from nc-dav's row helper (REQ §6.5 / §4.8) so the per-uid
/// TTL cache covers it.  `oc_accounts.data` is a JSON
/// `{"displayname":{"value":…,"scope":…,"verified":"0"},…}`; the AccountManager
/// syncs it from the backend but it can lag, hence users-first ordering.
async fn lookup_user_display_name(pool: &DbPool, prefix: &str, uid: &str) -> String {
    let users_sql = format!("SELECT displayname FROM {prefix}users WHERE uid = $1");
    if let Ok(Some(dn)) = sqlx::query_scalar::<_, Option<String>>(&users_sql)
        .bind(uid)
        .fetch_optional(pool)
        .await
    {
        if let Some(dn) = dn {
            if !dn.is_empty() {
                return dn;
            }
        }
    }

    let accounts_sql = format!("SELECT data FROM {prefix}accounts WHERE uid = $1");
    let accounts_data: Option<String> = sqlx::query_scalar(&accounts_sql)
        .bind(uid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    if let Some(ref data) = accounts_data {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(dn) = parsed
                .get("displayname")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                return dn.to_string();
            }
        }
    }

    uid.to_string()
}

/// The resolved client identity for one request (Phase 15 F2) — shared by
/// the auth middleware (throttle key, SameSite scheme) and the FastCGI proxy
/// (`REMOTE_ADDR` / `SERVER_NAME` / `SERVER_PORT` / `HTTPS` params), which is
/// why it lives in `nc-auth` rather than the server crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// The client IP: peer address, or the rightmost-untrusted
    /// `X-Forwarded-For` entry when the peer is a trusted proxy.
    pub ip: std::net::IpAddr,
    /// Effective scheme (`X-Forwarded-Proto` from a trusted proxy, or
    /// `overwriteprotocol` when the overwrite condition matches).
    pub https: bool,
    /// Effective host (`X-Forwarded-Host` / `overwritehost` from a trusted
    /// proxy, else `Host`).
    pub host: String,
    /// Numeric port from the host authority, else the scheme default.
    pub port: u16,
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
        sqlx::query(
            "CREATE TABLE oc_users (uid VARCHAR(64) NOT NULL PRIMARY KEY, displayname VARCHAR(64))",
        )
        .execute(&pool)
        .await
        .expect("users");
        sqlx::query(
            "CREATE TABLE oc_accounts (uid VARCHAR(64) NOT NULL PRIMARY KEY, data TEXT NOT NULL DEFAULT '{}')",
        )
        .execute(&pool)
        .await
        .expect("accounts");
        sqlx::query(
            "CREATE TABLE oc_appconfig (appid VARCHAR(32) NOT NULL DEFAULT '', configkey VARCHAR(64) NOT NULL DEFAULT '', configvalue TEXT NOT NULL DEFAULT '')",
        )
        .execute(&pool)
        .await
        .expect("appconfig");
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
        sqlx::query("INSERT INTO oc_users (uid, displayname) VALUES ('alice', 'Alice Example')")
            .execute(&pool)
            .await
            .expect("user");
        // Sharing allow-listed to a group alice is NOT in ('staff'), so
        // sharing is disabled for her (allow mode: in_allowed == false).
        sqlx::query(
            "INSERT INTO oc_appconfig (appid, configkey, configvalue) VALUES \
             ('core', 'shareapi_exclude_groups', 'allow'), \
             ('core', 'shareapi_exclude_groups_list', '[\"staff\"]')",
        )
        .execute(&pool)
        .await
        .expect("appconfig");

        let s = cached_user_state("alice", &pool, prefix).await.expect("resolve");
        assert!(s.is_admin, "admin membership read");
        assert!(s.twofa_enabled, "2FA provider enabled");
        assert!(s.sharing_disabled, "allow mode, alice not in staff → disabled");
        assert_eq!(s.display_name, "Alice Example", "oc_users displayname");

        // Cache serves: flip the tables in the DB and the next call must
        // return the cached values (60 s TTL).
        sqlx::query("DELETE FROM oc_group_user").execute(&pool).await.expect("del");
        sqlx::query("UPDATE oc_twofactor_providers SET enabled = 0")
            .execute(&pool)
            .await
            .expect("disable");
        sqlx::query("DELETE FROM oc_users").execute(&pool).await.expect("del users");
        let s2 = cached_user_state("alice", &pool, prefix).await.expect("cached");
        assert!(s2.is_admin, "cached admin state");
        assert!(s2.twofa_enabled, "cached 2FA state");
        assert!(s2.sharing_disabled, "cached sharing state");
        assert_eq!(s2.display_name, "Alice Example", "cached display name");

        // Unknown uid: no admin/2FA, display name falls back to uid; the
        // global allow list still applies (bob is in no allowed group).
        let s3 = cached_user_state("bob", &pool, prefix).await.expect("bob");
        assert!(!s3.is_admin);
        assert!(!s3.twofa_enabled);
        assert!(s3.sharing_disabled, "allow mode, bob in no allowed group");
        assert_eq!(s3.display_name, "bob");
    }

    #[tokio::test]
    async fn display_name_falls_back_to_accounts_json() {
        let pool = fresh_db().await;
        let prefix = "oc_";
        // No oc_users row; oc_accounts.data carries the displayname JSON.
        sqlx::query(
            "INSERT INTO oc_accounts (uid, data) VALUES ('carol', '{\"displayname\":{\"value\":\"Carol Chen\",\"scope\":\"v2-local\",\"verified\":\"0\"}}')",
        )
        .execute(&pool)
        .await
        .expect("accounts");
        let s = cached_user_state("carol", &pool, prefix).await.expect("resolve");
        assert_eq!(s.display_name, "Carol Chen", "oc_accounts JSON fallback");
        assert!(!s.is_admin);
        assert!(!s.twofa_enabled);
    }
}
