/// Nextcloud configuration loaded from `config/config.php` or a TOML fallback.
///
/// Field names match the keys used in Nextcloud's PHP config array.
/// All fields not explicitly required at startup are `Option<T>` so the struct
/// can be constructed from a partial config (e.g. a fresh install).
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// A config value marked **sensitive** by PHP (`lib/private/SystemConfig.php`)
/// whose `Debug` implementation redacts the contents, so it can never leak into
/// logs or error responses even via a blanket `{:?}` (REQ §17).
///
/// Use for secrets such as `preview_imaginary_url` / `preview_imaginary_key`.
/// The inner value is accessible through [`Sensitive::expose`].
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Sensitive(String);

impl Sensitive {
    /// Access the raw secret value.  Callers must take care never to log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Sensitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sensitive(<redacted>)")
    }
}

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
    /// Snowflake server id (`serverid` system config, §11.5).  When unset or ≤ 0,
    /// the snowflake generator falls back to `crc32(hostname)` (PHP
    /// `SnowflakeGenerator::getServerId`).  Masked to 9 bits at use.
    #[serde(default)]
    pub serverid: Option<i64>,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub maintenance: bool,
    /// Instance secret for token hashing and other cryptographic operations.
    /// PHP: `$CONFIG['secret']` — read via `SystemConfig::getValue('secret')`,
    /// used as `hash('sha512', $token . $secret)` in `PublicKeyTokenProvider`.
    /// Auto-generated on install.  Required for correct auth-token verification.
    #[serde(default)]
    pub secret: Option<String>,
    pub version: Option<String>,
    pub trusted_domains: Option<Vec<String>>,

    // ── Public URL ──────────────────────────────────────────────────────────
    #[serde(rename = "overwrite.cli.url")]
    pub overwrite_cli_url: Option<String>,

    // ── Auth / security ─────────────────────────────────────────────────────
    /// Legacy password salt (`passwordsalt`).  PHP
    /// `Hasher::legacyHashVerify()` mixes it into pre-versioning PHPass/bcrypt
    /// hashes (`password_verify($message . $salt, $hash)`).  Irrelevant for any
    /// version-prefixed hash (the only kind a modern install stores) — threaded
    /// through only so the legacy verification path stays faithful.  PHP reads
    /// it via `SystemConfig::getValue('passwordsalt', '')`, defaulting to empty.
    #[serde(default)]
    pub passwordsalt: Option<String>,
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

    // ── Preview / thumbnail (§10.12) ──────────────────────────────────────────
    /// Whether the preview system is enabled.
    /// Key: `enable_previews` in `config.php`.  Default: `true`.
    #[serde(default = "default_true")]
    pub enable_previews: bool,
    /// Path to the ffmpeg binary for video previews.
    /// Key: `preview_ffmpeg_path`.  When absent, video previews are unavailable.
    pub preview_ffmpeg_path: Option<String>,
    /// Path to the LibreOffice binary for office document previews.
    /// Key: `preview_libreoffice_path`.  When absent, office previews are unavailable.
    pub preview_libreoffice_path: Option<String>,
    /// Provider classes enabled for preview generation (§11.1).
    /// Key: `enabledPreviewProviders`.  When absent (`None`), PHP's default set
    /// applies — `MarkDown, TXT, OpenDocument, PNG, JPEG, GIF, BMP, XBitmap,
    /// Krita, WebP` (`PreviewManager::getEnabledDefaultProvider`).  Values are
    /// fully-qualified provider class names, e.g. `OC\Preview\Movie`.
    #[serde(rename = "enabledPreviewProviders", default)]
    pub enabled_preview_providers: Option<Vec<String>>,
    /// Imaginary server URL for out-of-process image generation (§11.4).
    /// Key: `preview_imaginary_url`.  **Sensitive** (`SystemConfig.php:42`) —
    /// redacted from all logs/debug output (REQ §17).  PHP's default `'invalid'`
    /// (and the empty string) mean "not configured".
    #[serde(rename = "preview_imaginary_url", default)]
    pub preview_imaginary_url: Option<Sensitive>,
    /// Imaginary API key (`preview_imaginary_key`) — sent as a **query parameter**
    /// on the Imaginary `/pipeline` request (§11.4).  **Sensitive**
    /// (`SystemConfig.php:43`) — redacted from all logs/debug output (REQ §17).
    /// Default: empty string (an Imaginary server with no key).
    #[serde(rename = "preview_imaginary_key", default)]
    pub preview_imaginary_key: Option<Sensitive>,
    /// Output format override (`preview_format`).  Only `webp` is honoured — a
    /// one-way override of the source-mime-mapped output (§11.4); any other value
    /// (default `jpeg`) leaves the per-source mapping in effect.
    #[serde(rename = "preview_format", default)]
    pub preview_format: Option<String>,
    /// Max preview width (`preview_max_x`).  Default `4096` (applied downstream).
    #[serde(rename = "preview_max_x", default)]
    pub preview_max_x: Option<i64>,
    /// Max preview height (`preview_max_y`).  Default `4096` (applied downstream).
    #[serde(rename = "preview_max_y", default)]
    pub preview_max_y: Option<i64>,
    /// Source size cap for image generation, in MiB (`preview_max_filesize_image`).
    /// Default `50`; `-1` disables the cap.  Sources larger than this are rejected
    /// before the Imaginary POST (§11.4 → 404, PHP `NotFoundException`).
    #[serde(rename = "preview_max_filesize_image", default)]
    pub preview_max_filesize_image: Option<i64>,
    /// Max concurrent preview *generations* (`preview_concurrency_new`).  Default:
    /// hardware concurrency, fallback `4` (PHP `Generator::getNumConcurrentPreviews`).
    #[serde(rename = "preview_concurrency_new", default)]
    pub preview_concurrency_new: Option<i64>,
    /// Max concurrent preview requests overall (`preview_concurrency_all`).  Default:
    /// 2× hardware concurrency, fallback `8`, clamped `≥ preview_concurrency_new`.
    /// The Rust fast path does not replicate this outer gate (hits need no admission
    /// control); it is read only for sizing parity / the deviation note.
    #[serde(rename = "preview_concurrency_all", default)]
    pub preview_concurrency_all: Option<i64>,

    // ── FastCGI / PHP-FPM dispatch (§7 Phase 7) ────────────────────────────────
    /// Unix socket path for PHP-FPM proxy dispatch (Phase 7).
    /// Key: `fastcgi_socket` in `config.php`, e.g. `/run/nc-fpm.sock`.
    /// When absent (default) the PHP-FPM fallback handler returns `502`.
    pub fastcgi_socket: Option<PathBuf>,
    /// FastCGI request timeout in milliseconds.
    /// Key: `fastcgi_timeout_ms`. Default: `30000`.
    #[serde(default = "default_fastcgi_timeout_ms")]
    pub fastcgi_timeout_ms: u64,

    // ── PHP CLI ────────────────────────────────────────────────────────────────
    /// PHP CLI interpreter for parsing `config.php` and the imagick startup
    /// probe.  Key: `php_binary` (path or `$PATH` name).  Default: `php`.
    /// `NC_PHP_BINARY` env var overrides — the only way to change the
    /// interpreter that parses `config.php` itself.  See [`resolve_php_binary`].
    pub php_binary: Option<String>,

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

/// Resolve the PHP CLI interpreter for `config.php` parsing and the imagick
/// startup probe: `NC_PHP_BINARY` env var → `php_binary` config key → `"php"`.
/// Empty values are treated as unset.
pub fn resolve_php_binary(config_value: Option<&str>) -> String {
    resolve_php_binary_from(config_value, std::env::var("NC_PHP_BINARY").ok().as_deref())
}

/// Pure core of [`resolve_php_binary`], parameterised for testability.
fn resolve_php_binary_from(config_value: Option<&str>, env_override: Option<&str>) -> String {
    if let Some(p) = env_override.map(str::trim).filter(|p| !p.is_empty()) {
        return p.to_string();
    }
    if let Some(p) = config_value.map(str::trim).filter(|p| !p.is_empty()) {
        return p.to_string();
    }
    "php".to_string()
}

// ── Loaders ─────────────────────────────────────────────────────────────────

impl NcConfig {
    /// Load from a Nextcloud `config.php` (PHP array syntax).
    /// Falls back to a TOML file if `config.php` is absent.
    pub fn load(base_dir: &Path) -> anyhow::Result<Self> {
        let php_path = base_dir.join("config/config.php");
        if php_path.exists() {
            // Use PHP CLI to convert config to JSON for reliable parsing
            // We read the file and extract just the $CONFIG array assignment.
            // The interpreter comes from `NC_PHP_BINARY` only — the `php_binary`
            // config key is not readable before this parse (bootstrap ordering).
            let php = resolve_php_binary(None);
            let abs_path = php_path.canonicalize()?;
            let json_output = std::process::Command::new(&php)
                .arg("-r")
                .arg(format!(
                    r#"
                    $CONFIG = [];
                    include '{}';
                    // Remove internal fields that shouldn't be serialized
                    unset($CONFIG['composerAutoloader']);
                    echo json_encode($CONFIG);
                    "#,
                    abs_path.display()
                ))
                .output();

            match json_output {
                Ok(output) if output.status.success() => {
                    let json_str = String::from_utf8_lossy(&output.stdout);
                    return Ok(serde_json::from_str(&json_str)?);
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!("{php}: PHP config parse failed: {}", stderr.trim());
                }
                Err(e) => {
                    tracing::debug!("config parse could not run {php} ({e}); falling back to the built-in parser");
                }
            }

            // Fallback to manual parsing if PHP CLI fails
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
        // Preview provider gating absent → PHP default set applies downstream.
        assert!(cfg.enabled_preview_providers.is_none());
        assert!(cfg.preview_imaginary_url.is_none());
    }

    #[test]
    fn preview_provider_config_parses() {
        let src = r#"<?php
$CONFIG = [
  'dbtype' => 'pgsql',
  'enabledPreviewProviders' => ['OC\Preview\PNG', 'OC\Preview\Movie', 'OC\Preview\Imaginary'],
  'preview_imaginary_url' => 'http://localhost:9090',
  'preview_ffmpeg_path' => '/usr/bin/ffmpeg',
];
"#;
        let cfg = NcConfig::from_php_config(src).expect("parse failed");
        let providers = cfg.enabled_preview_providers.as_ref().expect("providers");
        assert_eq!(providers.len(), 3);
        assert!(providers.contains(&"OC\\Preview\\PNG".to_string()));
        assert!(providers.contains(&"OC\\Preview\\Movie".to_string()));
        assert!(providers.contains(&"OC\\Preview\\Imaginary".to_string()));
        assert_eq!(
            cfg.preview_imaginary_url.as_ref().map(|s| s.expose()),
            Some("http://localhost:9090")
        );
        assert_eq!(cfg.preview_ffmpeg_path.as_deref(), Some("/usr/bin/ffmpeg"));

        // REQ §17: the sensitive URL must never appear in Debug output.
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("localhost:9090"), "URL leaked into Debug: {debug}");
        assert!(debug.contains("Sensitive(<redacted>)"));
    }

    #[test]
    fn php_binary_resolution() {
        // Env var wins over the config key.
        assert_eq!(
            resolve_php_binary_from(Some("/usr/bin/php-legacy"), Some("/opt/php")),
            "/opt/php"
        );
        // Config key used when the env var is absent.
        assert_eq!(
            resolve_php_binary_from(Some("php-legacy"), None),
            "php-legacy"
        );
        // Default when neither is set.
        assert_eq!(resolve_php_binary_from(None, None), "php");
        // Empty / whitespace-only values are treated as unset.
        assert_eq!(resolve_php_binary_from(Some(""), Some("  ")), "php");
        assert_eq!(resolve_php_binary_from(Some("  "), Some("")), "php");
        assert_eq!(
            resolve_php_binary_from(Some("php-legacy"), Some("")),
            "php-legacy"
        );
        // Values are trimmed.
        assert_eq!(
            resolve_php_binary_from(Some(" /usr/bin/php "), None),
            "/usr/bin/php"
        );
    }
}
