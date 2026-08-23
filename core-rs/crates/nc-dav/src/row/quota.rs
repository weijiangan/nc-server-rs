use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use super::filecache::lookup_by_path;


/// The user's free quota space, mirroring PHP's `Quota::free_space`
/// (Quota.php:74-89): `max(quota - used, 0)` bytes, or `None` when unlimited.
///
/// The quota comes from `oc_preferences` (`files` / `quota`): `none` /
/// `default` → unlimited, a plain number → bytes (human formats such as
/// "1 GB" are not parsed — the differential scenarios use byte values).  The
/// used size is the home storage root's cached size (PHP `getSize(sizeRoot)`).
pub async fn quota_free_space(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    storage_id: i64,
) -> Option<i64> {
    let sql = format!(
        "SELECT configvalue FROM {prefix}preferences \
         WHERE userid = $1 AND appid = 'files' AND configkey = 'quota'"
    );
    let quota = db_dispatch!(pool, |Db, c| {
        sqlx::query_scalar::<Db, String>(&sql)
            .bind(uid)
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
    });
    let quota = match quota.as_deref() {
        None | Some("none") | Some("default") => return None,
        Some(v) => parse_quota_bytes(v)?,
    };
    let used = lookup_by_path(pool, prefix, storage_id, "")
        .await
        .map(|r| r.size)
        .unwrap_or(0);
    Some((quota - used).max(0))
}


/// Parse a quota preference into bytes.  PHP stores quotas in its human
/// format — the OCS provisioning writes `"100 B"`, `"1.5 GB"`, … — plus bare
/// numbers; `"none"`/`"default"` mean unlimited.
pub(crate) fn parse_quota_bytes(v: &str) -> Option<i64> {
    let v = v.trim();
    if let Ok(n) = v.parse::<i64>() {
        return Some(n);
    }
    let lower = v.to_ascii_lowercase();
    let (num, mult) = if let Some(rest) = lower.strip_suffix("tb") {
        (rest, 1i64 << 40)
    } else if let Some(rest) = lower.strip_suffix("gb") {
        (rest, 1i64 << 30)
    } else if let Some(rest) = lower.strip_suffix("mb") {
        (rest, 1i64 << 20)
    } else if let Some(rest) = lower.strip_suffix("kb") {
        (rest, 1i64 << 10)
    } else if let Some(rest) = lower.strip_suffix("b") {
        (rest, 1)
    } else {
        return None;
    };
    let n: f64 = num.trim().parse().ok()?;
    Some((n * mult as f64) as i64)
}
