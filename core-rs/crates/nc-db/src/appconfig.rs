use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use crate::db_dispatch;
use crate::pool::DbPool;

/// Typed config value as stored in `oc_appconfig`.
#[derive(Debug, Clone)]
pub enum ConfigValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<String>),
}

/// Process-lifetime cache of non-lazy `oc_appconfig` rows.
///
/// Populated once at startup from:
///   `SELECT appid, configkey, configvalue, type FROM oc_appconfig WHERE lazy = 0`
///
/// Invalidated per-key on any write. The `maintenance` flag is read from
/// this cache on every request — no DB round trip.
#[derive(Debug, Default, Clone)]
pub struct AppConfigCache {
    /// (appid, configkey) → raw string value
    values: HashMap<(String, String), String>,
}

pub type SharedAppConfigCache = Arc<RwLock<AppConfigCache>>;

impl AppConfigCache {
    pub fn get_raw(&self, app: &str, key: &str) -> Option<&str> {
        self.values
            .get(&(app.to_string(), key.to_string()))
            .map(String::as_str)
    }

    pub fn get_string(&self, app: &str, key: &str) -> Option<String> {
        self.get_raw(app, key).map(str::to_string)
    }

    pub fn get_bool(&self, app: &str, key: &str) -> bool {
        match self.get_raw(app, key) {
            Some("1") | Some("true") | Some("yes") => true,
            _ => false,
        }
    }

    pub fn get_int(&self, app: &str, key: &str) -> Option<i64> {
        self.get_raw(app, key)?.parse().ok()
    }

    /// Convenience — shorthand used on every request.
    pub fn is_maintenance(&self) -> bool {
        self.get_bool("core", "maintenance")
    }

    /// Insert or update a single entry (called by write path to keep cache hot).
    pub fn set_raw(&mut self, app: &str, key: &str, value: String) {
        self.values
            .insert((app.to_string(), key.to_string()), value);
    }

    /// Remove an entry (called when a key is deleted from `oc_appconfig`).
    pub fn remove(&mut self, app: &str, key: &str) {
        self.values.remove(&(app.to_string(), key.to_string()));
    }

    /// Return all values stored under `app` whose key starts with `key_prefix`.
    ///
    /// Used by the brute-force allowlist scanner to collect all `whitelist_*`
    /// entries without needing to know how many exist or whether they are
    /// contiguous.
    pub fn values_with_prefix(&self, app: &str, key_prefix: &str) -> Vec<String> {
        self.values
            .iter()
            .filter(|((a, k), _)| a == app && k.starts_with(key_prefix))
            .map(|(_, v)| v.clone())
            .collect()
    }
}

/// Load non-lazy `oc_appconfig` rows into a shared cache.
pub async fn load_appconfig_cache(
    pool: &DbPool,
    table_prefix: &str,
) -> anyhow::Result<SharedAppConfigCache> {
    let table = format!("{table_prefix}appconfig");
    let rows: Vec<(String, String, Option<String>)> = db_dispatch!(pool, |Db, c| {
        sqlx::query_as::<Db, (String, String, Option<String>)>(&format!(
            "SELECT appid, configkey, configvalue FROM {table} WHERE lazy = 0"
        ))
        .fetch_all(c)
        .await?
    });

    let mut cache = AppConfigCache::default();
    for (app, key, val) in rows {
        if let Some(v) = val {
            cache.values.insert((app, key), v);
        }
    }

    tracing::info!(count = cache.values.len(), "App config cache loaded");
    Ok(Arc::new(RwLock::new(cache)))
}

/// Re-query all non-lazy `oc_appconfig` rows from the database and replace
/// the contents of `cache` in-place with the fresh data.
///
/// Called by the background capability-refresh task (Phase 7.7) on a periodic
/// interval so that `oc_appconfig` writes made via PHP-FPM are reflected in the
/// capability payload within one refresh cycle.  The existing `Arc` is not
/// replaced — only the inner `AppConfigCache` value is swapped, so all holders
/// of the shared reference see the fresh data immediately on their next read.
pub async fn reload_appconfig_cache(
    pool: &DbPool,
    table_prefix: &str,
    cache: &SharedAppConfigCache,
) -> anyhow::Result<()> {
    let table = format!("{table_prefix}appconfig");
    let rows: Vec<(String, String, Option<String>)> = db_dispatch!(pool, |Db, c| {
        sqlx::query_as::<Db, (String, String, Option<String>)>(&format!(
            "SELECT appid, configkey, configvalue FROM {table} WHERE lazy = 0"
        ))
        .fetch_all(c)
        .await?
    });

    let mut new_data = AppConfigCache::default();
    for (app, key, val) in rows {
        if let Some(v) = val {
            new_data.values.insert((app, key), v);
        }
    }

    let count = new_data.values.len();
    *cache.write().expect("appconfig reload write lock") = new_data;
    tracing::debug!(count, "App config cache reloaded");
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cache(entries: &[(&str, &str, &str)]) -> SharedAppConfigCache {
        let mut c = AppConfigCache::default();
        for (app, key, val) in entries {
            c.set_raw(app, key, val.to_string());
        }
        Arc::new(RwLock::new(c))
    }

    #[test]
    fn get_bool_maintenance() {
        let cache = make_cache(&[("core", "maintenance", "1")]);
        let guard = cache.read().unwrap();
        assert!(guard.is_maintenance());
        assert!(guard.get_bool("core", "maintenance"));
    }

    #[test]
    fn get_bool_false_values() {
        let cache = make_cache(&[("core", "maintenance", "0")]);
        let guard = cache.read().unwrap();
        assert!(!guard.is_maintenance());
    }

    #[test]
    fn get_int() {
        let cache = make_cache(&[("core", "loglevel", "2")]);
        let guard = cache.read().unwrap();
        assert_eq!(guard.get_int("core", "loglevel"), Some(2));
    }

    #[test]
    fn get_string() {
        let cache = make_cache(&[("core", "version", "30.0.2.1")]);
        let guard = cache.read().unwrap();
        assert_eq!(
            guard.get_string("core", "version").as_deref(),
            Some("30.0.2.1")
        );
    }

    #[test]
    fn absent_key_returns_none_or_false() {
        let cache = make_cache(&[]);
        let guard = cache.read().unwrap();
        assert_eq!(guard.get_string("core", "version"), None);
        assert!(!guard.get_bool("core", "maintenance"));
        assert_eq!(guard.get_int("core", "loglevel"), None);
    }

    #[test]
    fn live_update_via_write_lock() {
        let cache = make_cache(&[("core", "maintenance", "0")]);
        {
            let mut guard = cache.write().unwrap();
            guard.set_raw("core", "maintenance", "1".to_string());
        }
        let guard = cache.read().unwrap();
        assert!(guard.is_maintenance());
    }

    #[test]
    fn values_with_prefix_returns_matching_values() {
        let cache = make_cache(&[
            ("bruteForce", "whitelist_0", "10.0.0.0/24"),
            ("bruteForce", "whitelist_5", "192.168.0.0/24"), // non-contiguous key
            ("bruteForce", "other_key", "ignored"),
            ("core", "whitelist_0", "also_ignored"), // wrong app
        ]);
        let guard = cache.read().unwrap();
        let mut entries = guard.values_with_prefix("bruteForce", "whitelist_");
        entries.sort(); // HashMap order is non-deterministic
        assert_eq!(entries, vec!["10.0.0.0/24", "192.168.0.0/24"]);
    }

    #[test]
    fn values_with_prefix_empty_when_no_match() {
        let cache = make_cache(&[("core", "version", "30.0.2.1")]);
        let guard = cache.read().unwrap();
        assert!(guard
            .values_with_prefix("bruteForce", "whitelist_")
            .is_empty());
    }
}
