/// Nextcloud configuration loaded from `config/config.php` or a TOML fallback.
///
/// Field names match the keys used in Nextcloud's PHP config array.
/// All fields not explicitly required at startup are `Option<T>` so the struct
/// can be constructed from a partial config (e.g. a fresh install).
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct NcConfig {
    // ── Database ────────────────────────────────────────────────────────────
    /// `pgsql` or `sqlite3`
    pub dbtype: DbType,
    /// Host (and optional `:port`)
    pub dbhost: Option<String>,
    pub dbname: Option<String>,
    pub dbuser: Option<String>,
    pub dbpassword: Option<String>,
    /// Table prefix — default `oc_`
    #[serde(default = "default_table_prefix")]
    pub dbtableprefix: String,

    // ── Storage ─────────────────────────────────────────────────────────────
    /// Absolute path to the Nextcloud data directory
    pub datadirectory: Option<PathBuf>,

    // ── Instance ────────────────────────────────────────────────────────────
    pub instanceid: Option<String>,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub maintenance: bool,
    pub version: Option<String>,
    pub trusted_domains: Option<Vec<String>>,

    // ── Public URL ──────────────────────────────────────────────────────────
    #[serde(rename = "overwrite.cli.url")]
    pub overwrite_cli_url: Option<String>,

    // ── Auth / security ─────────────────────────────────────────────────────
    #[serde(
        rename = "auth.bruteforce.protection.enabled",
        default = "default_true"
    )]
    pub bruteforce_protection_enabled: bool,
    #[serde(rename = "oauth2.enable_oc_clients", default)]
    pub oauth2_enable_oc_clients: bool,

    // ── Caching / distributed ────────────────────────────────────────────────
    #[serde(rename = "memcache.distributed")]
    pub memcache_distributed: Option<String>,

    // ── Logging ─────────────────────────────────────────────────────────────
    /// 0=DEBUG, 1=INFO, 2=WARN, 3=ERROR, 4=FATAL
    #[serde(default = "default_loglevel")]
    pub loglevel: u8,
    pub logfile: Option<PathBuf>,

    // ── Misc ────────────────────────────────────────────────────────────────
    #[serde(rename = "data-fingerprint")]
    pub data_fingerprint: Option<String>,
    #[serde(default = "default_true")]
    pub bulkupload_enabled: bool,

    // ── FastCGI / PHP-FPM dispatch (§7 Phase 7) ────────────────────────────────
    /// Unix socket path for PHP-FPM proxy dispatch (Phase 7).
    /// Key: `fastcgi_socket` in `config.php`, e.g. `/run/nc-fpm.sock`.
    /// When absent (default) the PHP-FPM fallback handler returns `502`.
    pub fastcgi_socket: Option<PathBuf>,
    /// FastCGI request timeout in milliseconds.
    /// Key: `fastcgi_timeout_ms`. Default: `30000`.
    #[serde(default = "default_fastcgi_timeout_ms")]
    pub fastcgi_timeout_ms: u64,

    // ── Filename validation (§5.1) ───────────────────────────────────────────
    /// Exact filenames that are never allowed (e.g. `.htaccess`).
    /// Key: `forbidden_filenames` in `config.php`.  Default: `[".htaccess"]`.
    #[serde(default = "default_forbidden_filenames")]
    pub forbidden_filenames: Vec<String>,
    /// Forbidden basename prefixes (the part before the first non-leading dot).
    /// Key: `forbidden_filename_basenames`.  Default: `[]`.
    #[serde(default)]
    pub forbidden_filename_basenames: Vec<String>,
    /// Additional characters that must not appear in a filename.
    /// `\` and `/` are always forbidden regardless of this list.
    /// Key: `forbidden_filename_characters`.  Default: `[]`.
    #[serde(default)]
    pub forbidden_filename_characters: Vec<String>,
    /// Filename extensions (including the leading dot) that are never allowed.
    /// `.part` and `.filepart` are always forbidden regardless of this list.
    /// Key: `forbidden_filename_extensions`.  Default: `[".filepart"]`.
    #[serde(default = "default_forbidden_extensions")]
    pub forbidden_filename_extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbType {
    Pgsql,
    #[serde(alias = "sqlite3")]
    Sqlite,
}

fn default_table_prefix() -> String {
    "oc_".to_string()
}
fn default_true() -> bool {
    true
}
fn default_loglevel() -> u8 {
    1 // INFO
}
fn default_fastcgi_timeout_ms() -> u64 {
    30_000
}
fn default_forbidden_filenames() -> Vec<String> {
    vec![".htaccess".to_string()]
}
fn default_forbidden_extensions() -> Vec<String> {
    vec![".filepart".to_string()]
}

// ── Loaders ─────────────────────────────────────────────────────────────────

impl NcConfig {
    /// Load from a Nextcloud `config.php` (PHP array syntax).
    /// Falls back to a TOML file if `config.php` is absent.
    pub fn load(base_dir: &Path) -> anyhow::Result<Self> {
        let php_path = base_dir.join("config/config.php");
        if php_path.exists() {
            let src = std::fs::read_to_string(&php_path)?;
            return Self::from_php_config(&src)
                .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", php_path.display()));
        }

        let toml_path = base_dir.join("config/config.toml");
        if toml_path.exists() {
            let src = std::fs::read_to_string(&toml_path)?;
            return Ok(toml::from_str(&src)?);
        }

        anyhow::bail!(
            "No config found: expected {} or {}",
            php_path.display(),
            toml_path.display()
        );
    }

    /// Parse a `config.php`-style PHP array.
    ///
    /// Nextcloud's config files have the form:
    /// ```php
    /// <?php
    /// $CONFIG = [
    ///   'key' => 'value',
    ///   'arr' => ['a', 'b'],
    /// ];
    /// ```
    ///
    /// This parser handles the subset used by Nextcloud's config keys.
    /// It does not execute PHP — it extracts key/value pairs with regex.
    pub fn from_php_config(src: &str) -> Option<Self> {
        use std::collections::HashMap;

        let mut map: HashMap<String, PhpValue> = HashMap::new();

        // Strip comments and extract key => value pairs from the $CONFIG array body.
        let body = extract_config_body(src)?;
        parse_php_array_into(&body, &mut map);

        // Convert the flat map into our TOML-compatible representation so we can
        // reuse the serde::Deserialize impl via a JSON round-trip.
        let json = php_map_to_json(&map);
        serde_json::from_value(json).ok()
    }
}

// ── Internal PHP config parser ───────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) enum PhpValue {
    Str(String),
    Bool(bool),
    Int(i64),
    Float(f64),
    Array(Vec<PhpValue>),
}

/// Pull out the content between `[` and the terminal `];` of `$CONFIG = [...];`
fn extract_config_body(src: &str) -> Option<String> {
    let start = src.find("$CONFIG")?.checked_add("$CONFIG".len())?;
    let after = &src[start..];
    let bracket_start = after.find('[')?.checked_add(1)?;
    let body = &after[bracket_start..];

    // Find the matching closing bracket (depth-aware).
    let mut depth = 1usize;
    let mut end = 0;
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            '\'' | '"' => {
                // Skip string literal
                let q = chars[i];
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 1;
                    } else if chars[i] == q {
                        break;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    Some(body[..end].to_string())
}

fn parse_php_array_into(body: &str, map: &mut std::collections::HashMap<String, PhpValue>) {
    // Simple line-by-line extraction of `'key' => value,` patterns.
    // Handles string, bool, int, float, and single-level array values.
    // Multi-line arrays for `trusted_domains` etc. are handled by treating
    // `[...]` spans specially.
    let re_kv = regex_lite::Regex::new(
        r#"'([^']+)'\s*=>\s*((?:'[^']*'|"[^"]*"|\[[^\]]*\]|true|false|[0-9.\-]+))"#,
    )
    .expect("static regex");

    for cap in re_kv.captures_iter(body) {
        let key = cap[1].to_string();
        let val_str = cap[2].trim().to_string();
        let value = parse_php_value(&val_str);
        map.insert(key, value);
    }
}

fn parse_php_value(s: &str) -> PhpValue {
    if s == "true" {
        return PhpValue::Bool(true);
    }
    if s == "false" {
        return PhpValue::Bool(false);
    }
    if s.starts_with('\'') && s.ends_with('\'') {
        return PhpValue::Str(s[1..s.len() - 1].replace("\\'", "'"));
    }
    if s.starts_with('"') && s.ends_with('"') {
        return PhpValue::Str(s[1..s.len() - 1].replace("\\\"", "\""));
    }
    if s.starts_with('[') && s.ends_with(']') {
        // Parse inner array values (string elements only for now)
        let inner = &s[1..s.len() - 1];
        let elements: Vec<PhpValue> = inner
            .split(',')
            .filter_map(|e| {
                let t = e.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(parse_php_value(t))
                }
            })
            .collect();
        return PhpValue::Array(elements);
    }
    if let Ok(i) = s.parse::<i64>() {
        return PhpValue::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return PhpValue::Float(f);
    }
    PhpValue::Str(s.to_string())
}

fn php_map_to_json(map: &std::collections::HashMap<String, PhpValue>) -> serde_json::Value {
    use serde_json::{Map, Value};
    let mut obj = Map::new();
    for (k, v) in map {
        obj.insert(k.clone(), php_value_to_json(v));
    }
    Value::Object(obj)
}

fn php_value_to_json(v: &PhpValue) -> serde_json::Value {
    use serde_json::Value;
    match v {
        PhpValue::Str(s) => Value::String(s.clone()),
        PhpValue::Bool(b) => Value::Bool(*b),
        PhpValue::Int(i) => Value::Number((*i).into()),
        PhpValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        PhpValue::Array(arr) => Value::Array(arr.iter().map(php_value_to_json).collect()),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<?php
$CONFIG = [
  'dbtype' => 'pgsql',
  'dbhost' => 'localhost',
  'dbname' => 'nextcloud',
  'dbuser' => 'nc',
  'dbpassword' => 'secret',
  'dbtableprefix' => 'oc_',
  'datadirectory' => '/var/nc/data',
  'instanceid' => 'abc123',
  'installed' => true,
  'maintenance' => false,
  'version' => '30.0.2.1',
  'trusted_domains' => ['localhost', 'nextcloud.example.com'],
  'overwrite.cli.url' => 'https://nextcloud.example.com',
  'loglevel' => 1,
];
"#;

    #[test]
    fn parse_config_php() {
        let cfg = NcConfig::from_php_config(FIXTURE).expect("parse failed");
        assert_eq!(cfg.dbtype, DbType::Pgsql);
        assert_eq!(cfg.dbhost.as_deref(), Some("localhost"));
        assert_eq!(cfg.dbname.as_deref(), Some("nextcloud"));
        assert_eq!(cfg.dbuser.as_deref(), Some("nc"));
        assert_eq!(cfg.dbpassword.as_deref(), Some("secret"));
        assert_eq!(cfg.dbtableprefix, "oc_");
        assert_eq!(
            cfg.datadirectory.as_deref(),
            Some(std::path::Path::new("/var/nc/data"))
        );
        assert_eq!(cfg.instanceid.as_deref(), Some("abc123"));
        assert!(cfg.installed);
        assert!(!cfg.maintenance);
        assert_eq!(cfg.version.as_deref(), Some("30.0.2.1"));
        let domains = cfg.trusted_domains.as_ref().expect("domains");
        assert!(domains.contains(&"localhost".to_string()));
        assert!(domains.contains(&"nextcloud.example.com".to_string()));
        assert_eq!(
            cfg.overwrite_cli_url.as_deref(),
            Some("https://nextcloud.example.com")
        );
        assert_eq!(cfg.loglevel, 1);
    }

    #[test]
    fn defaults_applied_when_keys_absent() {
        let minimal = r#"<?php
$CONFIG = [
  'dbtype' => 'sqlite3',
];
"#;
        let cfg = NcConfig::from_php_config(minimal).expect("parse failed");
        assert_eq!(cfg.dbtype, DbType::Sqlite);
        assert_eq!(cfg.dbtableprefix, "oc_");
        assert!(!cfg.maintenance);
        assert!(cfg.bruteforce_protection_enabled);
        assert_eq!(cfg.loglevel, 1);
    }
}
