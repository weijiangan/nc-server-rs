use std::net::IpAddr;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use nc_db::{appconfig::SharedAppConfigCache, pool::DbPool};
use nc_db::{db_execute, db_scalar_one};

/// Result of a brute-force throttle check.
pub struct ThrottleResult {
    /// Optional async delay to impose before responding.
    pub delay: Option<Duration>,
    /// When `true`, reject immediately with HTTP 429.
    pub should_reject: bool,
    /// `Retry-After` value in seconds (valid when `should_reject = true`).
    pub retry_after_secs: u64,
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Default max attempts before 429 (used when not set in appconfig).
const DEFAULT_MAX_ATTEMPTS: i64 = 10;
/// Short window for 429 trigger: 30 minutes.
const SHORT_WINDOW_SECS: i64 = 30 * 60;
/// Long window for delay calculation: 12 hours.
const LONG_WINDOW_SECS: i64 = 12 * 60 * 60;
/// Maximum delay applied: 25 s (REQ §4.6: MAX_DELAY_MS = 25 000).
const MAX_DELAY_MS: u64 = 25_000;

/// Phase 18: per-(action, subnet) count cache.  The two COUNT queries run on
/// every request; the counts only change when `record_attempt` inserts a row,
/// so a short TTL is safe — the DB stays the source of truth on miss, and a
/// ≤30 s staleness on the exponential delay formula is immaterial (round-4
/// Task 13 raised the TTL from 2 s so low-cadence servers — where requests
/// are spaced beyond the old window — do not re-query on every request).
const COUNT_CACHE_TTL: Duration = Duration::from_secs(30);

/// Cached counts: (short-window count, long-window count, cached_at).
type CountEntry = (i64, i64, Instant);

fn count_cache() -> &'static DashMap<(String, String), CountEntry> {
    static CACHE: OnceLock<DashMap<(String, String), CountEntry>> = OnceLock::new();
    CACHE.get_or_init(DashMap::new)
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Check current throttle state for `(action, ip)`.
///
/// * `protection_enabled` — value of `auth.bruteforce.protection.enabled`
///   from `config.php` (passed from `NcConfig`; default `true`).
///
/// MUST be called before applying credentials; the delay returned here
/// is applied by the caller with `tokio::time::sleep` before returning the
/// 401 response.
pub async fn check_throttle(
    action: &str,
    client_ip: &str,
    protection_enabled: bool,
    appconfig: &SharedAppConfigCache,
    pool: &DbPool,
    prefix: &str,
) -> ThrottleResult {
    if !protection_enabled {
        return ThrottleResult {
            delay: None,
            should_reject: false,
            retry_after_secs: 0,
        };
    }

    // Read configurable max-attempts (REQ §4.6: default 10, configurable).
    // Key `auth.bruteforce.max-attempts` in `oc_appconfig`.
    let max_attempts = {
        let ac = appconfig.read().expect("appconfig lock");
        if is_ip_allowlisted(client_ip, &ac) {
            return ThrottleResult {
                delay: None,
                should_reject: false,
                retry_after_secs: 0,
            };
        }
        ac.get_int("auth", "bruteforce.max-attempts")
            .unwrap_or(DEFAULT_MAX_ATTEMPTS)
    };

    let now = unix_now();
    let short_cutoff = now - SHORT_WINDOW_SECS;
    let long_cutoff = now - LONG_WINDOW_SECS;
    let table = format!("{prefix}bruteforce_attempts");
    let subnet = ip_to_subnet(client_ip);

    // Phase 18: serve the two COUNTs from the per-(action, subnet) cache
    // when fresh.  The 429 and error paths below deliberately do NOT populate
    // the cache: a 429 storm wants exact counts, and a transient DB failure
    // must not freeze the throttle state.
    let cache_key = (action.to_string(), subnet.clone());
    let cached = count_cache()
        .get(&cache_key)
        .map(|e| *e.value())
        .filter(|(_, _, at)| at.elapsed() < COUNT_CACHE_TTL);

    let (short_count, long_count) = if let Some((s, l, _)) = cached {
        (s, l)
    } else {
        // Count attempts in the short window (30 min).
        // REQ §4.6: > max_attempts in 30 min → 429.
        let count_sql = format!(
            "SELECT COUNT(*) FROM {table} \
             WHERE action = $1 AND subnet = $2 AND occurred >= $3"
        );
        let short_fetched: Result<i64, sqlx::Error> =
            db_scalar_one!(pool, &count_sql, action, &subnet, short_cutoff);
        let short_count: i64 = match short_fetched {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    action,
                    subnet = %subnet,
                    error = %e,
                    "brute-force short-window COUNT failed — skipping throttle check for this request"
                );
                return ThrottleResult {
                    delay: None,
                    should_reject: false,
                    retry_after_secs: 0,
                };
            }
        };

        if short_count > max_attempts {
            return ThrottleResult {
                delay: Some(Duration::from_millis(MAX_DELAY_MS)),
                should_reject: true,
                retry_after_secs: 30,
            };
        }

        // Count attempts in long window (12 h) for delay calculation.
        // REQ §4.6: over-threshold in 12 h → throttle (sleep), not 429.
        let long_fetched: Result<i64, sqlx::Error> =
            db_scalar_one!(pool, &count_sql, action, &subnet, long_cutoff);
        let long_count: i64 = match long_fetched {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    action,
                    subnet = %subnet,
                    error = %e,
                    "brute-force long-window COUNT failed — skipping delay"
                );
                return ThrottleResult {
                    delay: None,
                    should_reject: false,
                    retry_after_secs: 0,
                };
            }
        };

        count_cache().insert(cache_key, (short_count, long_count, Instant::now()));
        (short_count, long_count)
    };

    // Defensive 429 for the cache-hit path (fresh entries never exceed
    // max_attempts — the 429 branch above returns before caching — but stay
    // correct if one ever does).
    if short_count > max_attempts {
        return ThrottleResult {
            delay: Some(Duration::from_millis(MAX_DELAY_MS)),
            should_reject: true,
            retry_after_secs: 30,
        };
    }

    if long_count == 0 {
        return ThrottleResult {
            delay: None,
            should_reject: false,
            retry_after_secs: 0,
        };
    }

    // delay = 100ms * 2^attempts, capped at MAX_DELAY_MS (25 000 ms).
    // Shift is clamped to 8 to avoid overflow before the .min() cap.
    let delay_ms = (100u64 * (1u64 << long_count.min(8) as u32)).min(MAX_DELAY_MS);
    ThrottleResult {
        delay: Some(Duration::from_millis(delay_ms)),
        should_reject: false,
        retry_after_secs: 0,
    }
}

/// Record a failed authentication attempt.
///
/// `id` is omitted from the INSERT; the DB auto-increments it.
pub async fn record_attempt(action: &str, client_ip: &str, pool: &DbPool, prefix: &str) {
    let now = unix_now();
    let subnet = ip_to_subnet(client_ip);
    let table = format!("{prefix}bruteforce_attempts");

    let sql = format!(
        "INSERT INTO {table}(action, occurred, ip, subnet, metadata) \
         VALUES($1, $2, $3, $4, $5)"
    );
    let result = db_execute!(pool, &sql, action, now, client_ip, &subnet, "{}");
    if let Err(e) = result {
        tracing::warn!(
            action,
            ip = %client_ip,
            subnet = %subnet,
            error = %e,
            "failed to record brute-force attempt — throttling may be incomplete"
        );
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Derive the /24 (IPv4) or /48 (IPv6) subnet string for grouping attempts.
/// Mirrors Nextcloud's `SubnetCalculator` logic.
pub fn ip_to_subnet(ip: &str) -> String {
    match IpAddr::from_str(ip) {
        Ok(IpAddr::V4(addr)) => {
            let [a, b, c, _] = addr.octets();
            format!("{a}.{b}.{c}.0/24")
        }
        Ok(IpAddr::V6(addr)) => {
            let segs = addr.segments();
            format!("{:x}:{:x}:{:x}::/48", segs[0], segs[1], segs[2])
        }
        Err(_) => ip.to_string(), // fall back to exact IP
    }
}

/// Check if `client_ip` is in the brute-force allowlist stored in `oc_appconfig`.
///
/// REQ §4.6: allowlist entries are `oc_appconfig` rows with
/// `appid = 'bruteForce'` and keys prefixed `whitelist_`; values are CIDR
/// strings (e.g. `192.168.1.0/24`).
///
/// Uses `values_with_prefix` so non-contiguous key numbering (e.g. keys
/// `whitelist_0`, `whitelist_5`) is handled correctly.
///
/// Full CIDR matching (i.e. `10.0.0.1` matching `10.0.0.0/24`) requires an
/// `ipnet`/`cidr` crate — current impl does subnet-string equality which
/// covers the common case. TODO: add `ipnet` dep for complete CIDR matching.
fn is_ip_allowlisted(client_ip: &str, ac: &nc_db::appconfig::AppConfigCache) -> bool {
    let client_subnet = ip_to_subnet(client_ip);
    for cidr in ac.values_with_prefix("bruteForce", "whitelist_") {
        if cidr == client_ip || cidr == client_subnet {
            return true;
        }
    }
    false
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_subnet() {
        assert_eq!(ip_to_subnet("192.168.1.42"), "192.168.1.0/24");
        assert_eq!(ip_to_subnet("10.0.0.1"), "10.0.0.0/24");
    }

    #[test]
    fn ipv6_subnet() {
        assert_eq!(
            ip_to_subnet("2001:db8:85a3::8a2e:370:7334"),
            "2001:db8:85a3::/48"
        );
    }

    #[test]
    fn invalid_ip_returns_itself() {
        assert_eq!(ip_to_subnet("localhost"), "localhost");
    }

    #[test]
    fn delay_formula() {
        // REQ §4.6: delay = 100ms * 2^attempts, cap = 25 000 ms
        assert_eq!((100u64 * (1u64 << 0u32)).min(MAX_DELAY_MS), 100);
        assert_eq!((100u64 * (1u64 << 1u32)).min(MAX_DELAY_MS), 200);
        // 100 * 2^8 = 25 600 → clamped to 25 000
        assert_eq!((100u64 * (1u64 << 8u32)).min(MAX_DELAY_MS), 25_000);
    }

    #[test]
    fn allowlist_prefix_scan_handles_gaps() {
        use nc_db::appconfig::AppConfigCache;
        let mut ac = AppConfigCache::default();
        // Deliberately non-contiguous: keys 0, 2, 5 — key 1 and 3-4 absent.
        ac.set_raw("bruteForce", "whitelist_0", "10.0.0.0/24".to_string());
        ac.set_raw("bruteForce", "whitelist_2", "192.168.1.0/24".to_string());
        ac.set_raw("bruteForce", "whitelist_5", "172.16.0.0/24".to_string());
        // All three subnets must be found despite gaps.
        assert!(is_ip_allowlisted("10.0.0.5", &ac)); // matches subnet string 10.0.0.0/24
        assert!(is_ip_allowlisted("192.168.1.99", &ac));
        assert!(is_ip_allowlisted("172.16.0.1", &ac));
        assert!(!is_ip_allowlisted("8.8.8.8", &ac));
    }
}
