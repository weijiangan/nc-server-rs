//! Quota enforcement for write operations (§5.2).
//!
//! Before any write (PUT, chunked upload assembly), the server computes the
//! user's free space and rejects the request with `507 Insufficient Storage`
//! if the upload would exceed the quota.
//!
//! ## Free-space calculation
//!
//! 1. Read the user's personal quota from `oc_preferences`
//!    (`appid='files'`, `configkey='quota'`).
//! 2. If the value is `"default"` (or absent), fall back to the server-wide
//!    `default_quota` from `oc_appconfig` (`appid='files'`).
//! 3. Parse the effective quota string:
//!    - `"none"` → `SPACE_UNLIMITED` (-3) → skip check, allow any write.
//!    - An integer string → bytes.
//!    - A human-readable string (`"1 GB"`, `"500 MB"`, …) → bytes.
//! 4. Query the `size` column of the `files/` root node in `oc_filecache`
//!    (Nextcloud keeps this as the recursive total for the home storage).
//! 5. `free_space = quota_bytes - used_bytes` (clamped at 0).
//! 6. Any negative free-space sentinel → skip check.
//!
//! The comparison is:
//! ```text
//! upload_size = max(Content-Length, X-Expected-Entity-Length, OC-Total-Length)
//! if upload_size > free_space → 507
//! ```

use nc_db::{appconfig::SharedAppConfigCache, pool::DbPool};

// ─── Sentinel values (PHP OC\Files\FileInfo) ─────────────────────────────────

/// File size is not yet computed (scanning still needed).
pub const SPACE_NOT_COMPUTED: i64 = -1;
/// Free space cannot be determined (filesystem stats unavailable).
pub const SPACE_UNKNOWN: i64 = -2;
/// No quota / unlimited storage.
pub const SPACE_UNLIMITED: i64 = -3;

// ─── Public API ───────────────────────────────────────────────────────────────

/// Check whether uploading `upload_bytes` fits within the user's quota.
///
/// - Returns `Ok(())` when the write is permitted.
/// - Returns `Err(())` when the write would exceed the quota; the caller
///   should respond with `507 Insufficient Storage`.
///
/// When `upload_bytes <= 0` (no size information available), the check is
/// skipped and the write is allowed.  Any negative `free_space()` value is
/// treated as unlimited and also skips the check.
pub async fn check_quota(
    pool: &DbPool,
    prefix: &str,
    appconfig_cache: &SharedAppConfigCache,
    uid: &str,
    storage_id: i64,
    upload_bytes: i64,
) -> Result<(), ()> {
    // No size information → can't check; allow.
    if upload_bytes <= 0 {
        return Ok(());
    }

    let free = compute_free_space(pool, prefix, appconfig_cache, uid, storage_id).await;

    // Any negative sentinel → unlimited / unknown → skip check.
    if free < 0 {
        return Ok(());
    }

    if upload_bytes > free {
        tracing::debug!(
            uid, upload_bytes, free,
            "§5.2 quota check: upload exceeds available space"
        );
        Err(())
    } else {
        Ok(())
    }
}

/// Compute the free space in bytes available for `uid` in their home storage.
///
/// Returns a **non-negative** byte count when a finite quota is in effect, or
/// a **negative sentinel** (`SPACE_UNLIMITED`, `SPACE_UNKNOWN`) when the check
/// should be skipped.
pub async fn compute_free_space(
    pool: &DbPool,
    prefix: &str,
    appconfig_cache: &SharedAppConfigCache,
    uid: &str,
    storage_id: i64,
) -> i64 {
    // 1. Resolve the effective quota string for this user.
    let effective = resolve_effective_quota(pool, prefix, appconfig_cache, uid).await;

    // 2. Parse it to bytes.  "none" or unparseable → unlimited.
    let quota_bytes = match parse_quota_string(&effective) {
        Some(b) => b,
        None => return SPACE_UNLIMITED,
    };

    // 3. Used bytes: size of the "files/" root node in oc_filecache.
    let used_bytes = lookup_used_bytes(pool, prefix, storage_id).await;

    // 4. free = quota - used, clamped at 0 (negative would be confusing here).
    quota_bytes.saturating_sub(used_bytes)
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Resolve the effective quota string for a user:
/// - Personal quota from `oc_preferences` if set (and not `"default"`)
/// - Server-wide `default_quota` from `oc_appconfig` otherwise
/// - Falls back to `"none"` (unlimited) when nothing is configured
async fn resolve_effective_quota(
    pool: &DbPool,
    prefix: &str,
    appconfig_cache: &SharedAppConfigCache,
    uid: &str,
) -> String {
    let personal = lookup_quota_preference(pool, prefix, uid).await;

    match personal.as_deref() {
        // Explicitly set to a specific value (not "default") → use it.
        Some(v) if !v.is_empty() && !v.eq_ignore_ascii_case("default") => v.to_string(),

        // Absent or "default" → read the server-wide default.
        _ => {
            appconfig_cache
                .read()
                .ok()
                .and_then(|g| g.get_string("files", "default_quota"))
                .unwrap_or_else(|| "none".to_string())
        }
    }
}

/// Query `oc_preferences` for the user's quota setting.
///
/// Returns `None` when no row exists (treat as "default").
async fn lookup_quota_preference(pool: &DbPool, prefix: &str, uid: &str) -> Option<String> {
    let sql = format!(
        "SELECT configvalue FROM {prefix}preferences \
         WHERE userid = $1 AND appid = 'files' AND configkey = 'quota'"
    );
    sqlx::query_scalar::<_, Option<String>>(&sql)
        .bind(uid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
}

/// Look up the number of bytes already used by the home storage.
///
/// Nextcloud keeps the `oc_filecache` `size` column of the `files/` root as a
/// running recursive total — no subtree scan needed.
async fn lookup_used_bytes(pool: &DbPool, prefix: &str, storage_id: i64) -> i64 {
    let path_hash = crate::row::path_hash("files");
    let sql = format!(
        "SELECT size FROM {prefix}filecache WHERE storage = $1 AND path_hash = $2"
    );
    sqlx::query_scalar::<_, Option<i64>>(&sql)
        .bind(storage_id)
        .bind(&path_hash)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .filter(|&s| s >= 0)
        .unwrap_or(0)
}

// ─── Quota string parser ──────────────────────────────────────────────────────

/// Parse a Nextcloud quota string to a byte count.
///
/// Handles:
/// - `"none"` → `None` (unlimited)
/// - An integer string (bytes) → `Some(n)`
/// - Human-readable: `"1 KB"`, `"1 MB"`, `"1 GB"`, `"1 TB"`, `"1 PB"` (powers of 1024)
/// - A float + unit (e.g. `"1.5 GB"`) → `Some(bytes)`
///
/// Returns `None` for unlimited or unrecognised formats.
pub fn parse_quota_string(s: &str) -> Option<i64> {
    let s = s.trim();

    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return None;
    }

    // Plain integer (already in bytes).
    if let Ok(n) = s.parse::<i64>() {
        return if n >= 0 { Some(n) } else { None };
    }

    // Human-readable: split on the last digit to separate number from unit.
    let split_pos = s.rfind(|c: char| c.is_ascii_digit())?;
    let (num_part, unit_part) = s.split_at(split_pos + 1);

    let num: f64 = num_part.trim().parse().ok()?;
    let multiplier: i64 = match unit_part.trim().to_uppercase().as_str() {
        "B" => 1,
        "KB" => 1 << 10,
        "MB" => 1 << 20,
        "GB" => 1 << 30,
        "TB" => 1 << 40,
        "PB" => 1 << 50,
        _ => return None,
    };

    let bytes = (num * multiplier as f64) as i64;
    if bytes >= 0 { Some(bytes) } else { None }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::parse_quota_string;

    #[test]
    fn none_is_unlimited() {
        assert_eq!(parse_quota_string("none"), None);
        assert_eq!(parse_quota_string("NONE"), None);
        assert_eq!(parse_quota_string(""), None);
    }

    #[test]
    fn integer_bytes() {
        assert_eq!(parse_quota_string("1073741824"), Some(1_073_741_824));
        assert_eq!(parse_quota_string("0"), Some(0));
    }

    #[test]
    fn negative_integer_is_unlimited() {
        assert_eq!(parse_quota_string("-1"), None);
        assert_eq!(parse_quota_string("-3"), None);
    }

    #[test]
    fn human_readable_units() {
        assert_eq!(parse_quota_string("1 KB"), Some(1 << 10));
        assert_eq!(parse_quota_string("1 MB"), Some(1 << 20));
        assert_eq!(parse_quota_string("1 GB"), Some(1 << 30));
        assert_eq!(parse_quota_string("1 TB"), Some(1 << 40));
        assert_eq!(parse_quota_string("1 PB"), Some(1 << 50));
    }

    #[test]
    fn bare_bytes_unit() {
        // The OCS provisioning API stores an integer quota as "<n> B"
        // (live-verified: PUT /ocs/v2.php/cloud/users/{uid} key=quota value=100
        // lands in oc_preferences as "100 B").
        assert_eq!(parse_quota_string("100 B"), Some(100));
        assert_eq!(parse_quota_string("0 B"), Some(0));
    }

    #[test]
    fn human_readable_fractional() {
        // 1.5 GB = 1.5 * 2^30 = 1_610_612_736
        let expected = (1.5_f64 * (1u64 << 30) as f64) as i64;
        assert_eq!(parse_quota_string("1.5 GB"), Some(expected));
    }

    #[test]
    fn human_readable_case_insensitive() {
        assert_eq!(parse_quota_string("512 mb"), Some(1 << 29));
    }

    #[test]
    fn unknown_unit_is_none() {
        assert_eq!(parse_quota_string("1 XB"), None);
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(parse_quota_string("  2 GB  "), Some(2 << 30));
    }
}
