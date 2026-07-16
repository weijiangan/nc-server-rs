## 18. Configuration File

The Rust server reads `config/config.php` (for compat with existing Nextcloud installations) or an equivalent TOML/YAML file for fresh installs. Required keys:

| Key | Type | Description |
|---|---|---|
| `dbtype` | string | `pgsql`, `mysql`, `sqlite3` |
| `dbhost` | string | DB host (+ optional `:port`) |
| `dbname` | string | DB name |
| `dbuser` | string | DB username |
| `dbpassword` | string | DB password |
| `dbtableprefix` | string | Default `oc_` |
| `datadirectory` | string | Absolute path to Nextcloud data dir |
| `installed` | bool | |
| `maintenance` | bool | |
| `version` | string | Nextcloud version string |
| `trusted_domains` | array | Allowed Host header values |
| `overwrite.cli.url` | string | Public base URL |
| `htaccess.IgnoreFrontController` | bool | Clean URLs active |
| `pollinterval` | int | Long-poll interval (default 60) |
| `auth.bruteforce.protection.enabled` | bool | Default true |
| `auth.bruteforce.allowlist` | array | IPs/subnets exempt from throttle |
| `memcache.distributed` | string | Distributed cache class name |
| `redis` | object | Redis connection config |
| `memcached_servers` | array | Memcached server list |
| `data-fingerprint` | string | Reported in DAV `{oc:}data-fingerprint` |
| `bulkupload.enabled` | bool | Default true |
| `oauth2.enable_oc_clients` | bool | Default false |
| `loglevel` | int | 0=DEBUG … 4=FATAL |
| `logfile` | string | Log file path |
| `instanceid` | string | Instance identifier (e.g., `oc1a2b3c4d5e`). Used as PHP `session_name()` — the session cookie is named after this value. Also used in `{oc:}id` DAV property (zero-padded fileid + instanceid). Auto-generated on first install as `'oc' + random(10)`. Required for session cookie detection. |
| `secret` | string | Server secret. Used in token hash: `hash('sha512', $token . $secret)` (`PublicKeyTokenProvider.php:414`). Required for all auth token lookups against `oc_authtoken`. Auto-generated on install. |

---

---

Prev: [`17-logging-and-observability.md`](17-logging-and-observability.md) · Up: [`README.md`](README.md) · Next: [`19-compatibility-test-matrix.md`](19-compatibility-test-matrix.md)
