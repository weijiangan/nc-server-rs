use std::sync::{Arc, RwLock};

use md5::{Digest, Md5};
use nc_db::appconfig::SharedAppConfigCache;

pub type SharedCapabilityCache = Arc<RwLock<CapabilityCache>>;

/// Pre-built OCS capability payloads — built once at startup and rebuilt when
/// any capability-affecting `oc_appconfig` key changes.
///
/// Two variants are stored (auth vs unauthenticated / public-only). For the
/// native capabilities (core, dav, files) both variants contain all three
/// blocks since all of them implement `IPublicCapability`. PHP-FPM-owned app
/// capabilities (e.g. `files_sharing`) are merged in via `apply_php_capabilities`
/// (Phase 7.7).
#[derive(Debug, Clone)]
pub struct CapabilityCache {
    /// The native data object (`version` + `capabilities` with core/dav/files).
    /// Stored so `apply_php_capabilities` can rebuild without re-reading config.
    pub native_data: serde_json::Value,
    /// PHP-app capabilities from an **authenticated** fetch (all `ICapability`
    /// results, e.g. `files_sharing`, `text`, …).  Merged into `auth_*` only.
    /// `Value::Null` until the Phase 7.7 fetch completes.
    pub php_app_capabilities: serde_json::Value,
    /// PHP-app capabilities from an **unauthenticated** fetch (`IPublicCapability`
    /// results only).  Merged into `public_*` only.  `Value::Null` until the
    /// Phase 7.7 public fetch completes.
    pub php_public_capabilities: serde_json::Value,
    /// Canonical version string from `config.php` (`NcConfig.version`), stored so
    /// `rebuild_capability_cache` can re-use it without access to the config struct.
    /// Falls back to `"0.0.0.0"` when config.php has no version key and
    /// `oc_appconfig` has no `core/oc_version` or `core/version` row.
    pub version: String,
    pub auth_json: String,
    pub auth_xml: String,
    pub auth_etag: String,
    /// Public-only subset (unauthenticated requests) — contains only
    /// `IPublicCapability` results from PHP apps.
    pub public_json: String,
    pub public_xml: String,
    pub public_etag: String,
}

impl CapabilityCache {
    /// Merge `php_caps` (the `data.capabilities` sub-object from the PHP OCS
    /// response via an **authenticated** fetch, e.g. `{"files_sharing": {...},
    /// "text": {...}}`) into the cache and rebuild `auth_*` representations.
    ///
    /// Keys in `php_caps` are shallow-merged at the `capabilities` level; PHP
    /// keys win on collision (in practice there is no overlap with the native
    /// `core`, `dav`, and `files` blocks).
    ///
    /// Only `auth_*` is updated.  Call [`apply_php_public_capabilities`] to
    /// update `public_*` with the `IPublicCapability`-only subset.
    pub fn apply_php_capabilities(&mut self, php_caps: serde_json::Value) {
        self.php_app_capabilities = php_caps;
        self.rebuild_serialized();
    }

    /// Merge `php_pub_caps` (the `data.capabilities` sub-object from an
    /// **unauthenticated** PHP OCS capabilities fetch, containing only
    /// `IPublicCapability` results) into the cache and rebuild `public_*`.
    ///
    /// Only `public_*` is updated; `auth_*` is left unchanged.
    pub fn apply_php_public_capabilities(&mut self, php_pub_caps: serde_json::Value) {
        self.php_public_capabilities = php_pub_caps;
        self.rebuild_serialized();
    }

    /// Rebuild `auth_*` and `public_*` serialized forms from `native_data`:
    /// - `auth_*`  = native + `php_app_capabilities`  (full authenticated set)
    /// - `public_*` = native + `php_public_capabilities` (IPublicCapability only)
    ///
    /// Called after any change to either PHP capability source.
    pub fn rebuild_serialized(&mut self) {
        // ── Authenticated variant ────────────────────────────────────────────
        let auth_merged = self.merge_caps(&self.php_app_capabilities.clone());
        let auth_json = serde_json::to_string(&auth_merged).unwrap();
        self.auth_etag = md5_etag(&auth_json);
        self.auth_xml = crate::envelope::json_to_xml_data(&auth_merged);
        self.auth_json = auth_json;

        // ── Public (unauthenticated) variant ─────────────────────────────────
        // Uses the separate IPublicCapability-only fetch result.  Falls back to
        // native-only when the public fetch has not yet completed (Null).
        let pub_merged = self.merge_caps(&self.php_public_capabilities.clone());
        let pub_json = serde_json::to_string(&pub_merged).unwrap();
        self.public_etag = md5_etag(&pub_json);
        self.public_xml = crate::envelope::json_to_xml_data(&pub_merged);
        self.public_json = pub_json;
    }

    /// Shallow-merge `php_caps` into a clone of `native_data` at the
    /// `capabilities` key level.  Returns the merged value.
    fn merge_caps(&self, php_caps: &serde_json::Value) -> serde_json::Value {
        let mut merged = self.native_data.clone();
        if let (Some(caps_obj), Some(php_obj)) = (
            merged
                .get_mut("capabilities")
                .and_then(|v| v.as_object_mut()),
            php_caps.as_object(),
        ) {
            for (k, v) in php_obj {
                caps_obj.insert(k.clone(), v.clone());
            }
        }
        merged
    }
}

/// Build the capability cache from the appconfig cache.
///
/// `version_override` — when `Some`, this version string is used instead of
/// looking up `oc_appconfig`.  It comes from `config.php`'s `$CONFIG['version']`
/// (`NcConfig.version`), which is the canonical source (the same source PHP's
/// `status.php` reads via `ServerVersion` → `version.php`).  `oc_appconfig`
/// is consulted only as a fallback on installs where `config.php` lacks the key.
///
/// Called at startup and after any config write that affects capabilities.
pub fn build_capability_cache(
    ac: &nc_db::appconfig::AppConfigCache,
    version_override: Option<&str>,
) -> CapabilityCache {
    // ── Version info ─────────────────────────────────────────────────────────
    let version_str = version_override
        .map(|v| v.to_string())
        .or_else(|| ac.get_string("core", "oc_version"))
        .or_else(|| ac.get_string("core", "version"))
        .unwrap_or_else(|| "0.0.0.0".to_string());

    let parts: Vec<u32> = version_str
        .split('.')
        .filter_map(|p| p.parse().ok())
        .collect();

    let major = parts.first().copied().unwrap_or(0);
    let minor = parts.get(1).copied().unwrap_or(0);
    let micro = parts.get(2).copied().unwrap_or(0);
    let edition = ac.get_string("core", "edition").unwrap_or_default();
    let extended_support = ac.get_bool("core", "extendedSupport");

    // ── Forbidden filenames (from appconfig cache) ───────────────────────────
    let forbidden_filenames: Vec<String> = ac
        .get_string("files", "forbidden_filenames")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let forbidden_basenames: Vec<String> = ac
        .get_string("files", "forbidden_filename_basenames")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let forbidden_chars: Vec<String> = ac
        .get_string("files", "forbidden_filename_characters")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let forbidden_extensions: Vec<String> = ac
        .get_string("files", "forbidden_filename_extensions")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // ── Build JSON data structure ─────────────────────────────────────────────
    let data = serde_json::json!({
        "version": {
            "major": major,
            "minor": minor,
            "micro": micro,
            "string": format!("{major}.{minor}.{micro}"),
            "edition": edition,
            "extendedSupport": extended_support,
        },
        "capabilities": {
            "core": {
                "pollinterval": 60,
                "webdav-root": "remote.php/webdav",
                "reference-api": true,
                "reference-regex": "",
                "mod-rewrite-working": true
            },
            "dav": {
                "chunking": "1.0",
                "public_shares_chunking": true,
                "search_supports_creation_time": true,
                "search_supports_upload_time": true,
                "bulkupload": "1.0"
            },
            "files": {
                "bigfilechunking": true,
                "blacklisted_files": [],
                "forbidden_filenames": forbidden_filenames,
                "forbidden_filename_basenames": forbidden_basenames,
                "forbidden_filename_characters": forbidden_chars,
                "forbidden_filename_extensions": forbidden_extensions,
                "chunked_upload": {
                    "max_size": 10_737_418_240i64,
                    "max_parallel_count": 5
                },
                "file_conversions": []
            }
        }
    });

    let json = serde_json::to_string(&data).unwrap();
    let etag = md5_etag(&json);
    let xml = crate::envelope::json_to_xml_data(&data);

    CapabilityCache {
        native_data: data,
        php_app_capabilities: serde_json::Value::Null,
        php_public_capabilities: serde_json::Value::Null,
        version: version_str,
        auth_json: json.clone(),
        auth_xml: xml.clone(),
        auth_etag: etag.clone(),
        // public_* starts as native-only (no PHP caps yet).
        // After startup, apply_php_public_capabilities() will merge the
        // IPublicCapability-only subset from the unauthenticated PHP fetch.
        public_json: json,
        public_xml: xml,
        public_etag: etag,
    }
}

/// Load a `SharedCapabilityCache` from the appconfig cache at startup.
///
/// `version_override` is the canonical version string from `config.php`
/// (`NcConfig.version`).  It takes precedence over `oc_appconfig` keys.
pub fn load_capability_cache(
    appconfig_cache: &SharedAppConfigCache,
    version_override: Option<&str>,
) -> SharedCapabilityCache {
    let ac = appconfig_cache.read().expect("appconfig lock poisoned");
    let cache = build_capability_cache(&ac, version_override);
    Arc::new(RwLock::new(cache))
}

/// Compute `md5(json_string)` — mirrors PHP's `md5(json_encode($result))`.
fn md5_etag(json: &str) -> String {
    let mut h = Md5::new();
    h.update(json.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_db::appconfig::AppConfigCache;

    #[test]
    fn builds_without_appconfig() {
        let empty = AppConfigCache::default();
        let cache = build_capability_cache(&empty, None);
        assert!(!cache.auth_etag.is_empty());
        assert!(cache.auth_json.contains("pollinterval"));
        assert!(cache.auth_json.contains("bigfilechunking"));
        assert!(cache.auth_json.contains("chunking"));
    }

    #[test]
    fn etag_is_md5_of_json() {
        let empty = AppConfigCache::default();
        let cache = build_capability_cache(&empty, None);
        let expected = {
            let mut h = Md5::new();
            h.update(cache.auth_json.as_bytes());
            format!("{:x}", h.finalize())
        };
        assert_eq!(cache.auth_etag, expected);
    }

    #[test]
    fn public_and_auth_match_before_php_fpm_merge() {
        let empty = AppConfigCache::default();
        let cache = build_capability_cache(&empty, None);
        assert_eq!(cache.auth_json, cache.public_json);
        assert_eq!(cache.auth_etag, cache.public_etag);
    }

    #[test]
    fn native_data_contains_capabilities_key() {
        let empty = AppConfigCache::default();
        let cache = build_capability_cache(&empty, None);
        assert!(cache.native_data.get("capabilities").is_some());
        assert!(cache.native_data.get("version").is_some());
        assert!(cache.php_app_capabilities.is_null());
        assert!(cache.php_public_capabilities.is_null());
    }

    #[test]
    fn apply_php_capabilities_updates_only_auth() {
        let empty = AppConfigCache::default();
        let mut cache = build_capability_cache(&empty, None);
        let native_etag = cache.auth_etag.clone();
        let native_public_json = cache.public_json.clone();

        let php_caps = serde_json::json!({
            "files_sharing": {
                "api_enabled": true,
                "public": {"enabled": true}
            }
        });
        cache.apply_php_capabilities(php_caps);

        // PHP capabilities present in auth variant.
        assert!(cache.auth_json.contains("files_sharing"));
        // Native capabilities still present in auth.
        assert!(cache.auth_json.contains("pollinterval"));
        assert!(cache.auth_json.contains("bigfilechunking"));
        // ETag changed for auth.
        assert_ne!(cache.auth_etag, native_etag);
        // public_* is NOT updated by apply_php_capabilities — it stays native-only
        // until apply_php_public_capabilities is called.
        assert_eq!(cache.public_json, native_public_json);
        assert!(!cache.public_json.contains("files_sharing"));
    }

    #[test]
    fn apply_php_public_capabilities_updates_only_public() {
        let empty = AppConfigCache::default();
        let mut cache = build_capability_cache(&empty, None);

        // First fill auth with full caps.
        let php_caps = serde_json::json!({"files_sharing": {"api_enabled": true}});
        cache.apply_php_capabilities(php_caps);

        let auth_json_before = cache.auth_json.clone();
        let auth_etag_before = cache.auth_etag.clone();

        // Now apply the public (IPublicCapability-only) subset.
        let pub_caps = serde_json::json!({"files_sharing": {"public": {"enabled": true}}});
        cache.apply_php_public_capabilities(pub_caps);

        // auth_* is unchanged.
        assert_eq!(cache.auth_json, auth_json_before);
        assert_eq!(cache.auth_etag, auth_etag_before);

        // public_* now contains the public subset.
        assert!(cache.public_json.contains("files_sharing"));
        // public_json should differ from auth_json (different php_caps content).
        assert_ne!(cache.public_json, cache.auth_json);
        // public_* contains native caps too.
        assert!(cache.public_json.contains("pollinterval"));
    }

    #[test]
    fn auth_and_public_differ_after_separate_merges() {
        let empty = AppConfigCache::default();
        let mut cache = build_capability_cache(&empty, None);

        // Simulate: authenticated fetch returns more capabilities than public.
        let auth_only_caps = serde_json::json!({
            "files_sharing": {"api_enabled": true},
            "admin_only_feature": {"enabled": true}
        });
        let pub_caps = serde_json::json!({
            "files_sharing": {"api_enabled": true}
        });

        cache.apply_php_capabilities(auth_only_caps);
        cache.apply_php_public_capabilities(pub_caps);

        assert!(cache.auth_json.contains("admin_only_feature"),
            "auth_* must include all authenticated PHP caps");
        assert!(!cache.public_json.contains("admin_only_feature"),
            "public_* must NOT include ICapability-only caps");
        assert!(cache.public_json.contains("files_sharing"),
            "public_* must include IPublicCapability caps");
    }

    #[test]
    fn version_override_takes_precedence_over_appconfig() {
        // When version_override is provided, it must be used even when
        // oc_appconfig has rows (simulating config.php providing the version).
        let mut ac = AppConfigCache::default();
        // Insert a value that must NOT be used — the override must win.
        ac.set_raw("core", "oc_version", "99.99.99.99".to_string());
        let cache = build_capability_cache(&ac, Some("34.0.1.2"));
        assert_eq!(cache.version, "34.0.1.2");
        let v = &cache.native_data["version"];
        assert_eq!(v["major"], 34);
        assert_eq!(v["minor"], 0);
        assert_eq!(v["micro"], 1);
        assert_eq!(v["string"], "34.0.1");
    }

    #[test]
    fn version_falls_back_to_appconfig() {
        let mut ac = AppConfigCache::default();
        ac.set_raw("core", "oc_version", "30.0.2.1".to_string());
        let cache = build_capability_cache(&ac, None);
        assert_eq!(cache.version, "30.0.2.1");
        let v = &cache.native_data["version"];
        assert_eq!(v["major"], 30);
        assert_eq!(v["minor"], 0);
        assert_eq!(v["micro"], 2);
    }

    #[test]
    fn rebuild_serialized_no_php_caps_unchanged() {
        let empty = AppConfigCache::default();
        let mut cache = build_capability_cache(&empty, None);
        let original_auth = cache.auth_json.clone();
        let original_pub = cache.public_json.clone();
        // Calling rebuild_serialized with no PHP caps should produce identical output.
        cache.rebuild_serialized();
        assert_eq!(cache.auth_json, original_auth);
        assert_eq!(cache.public_json, original_pub);
    }
}
