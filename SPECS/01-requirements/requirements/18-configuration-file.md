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
| `enable_previews` | bool | Preview generation/serving master switch. Default `true`. Drives `{nc:}has-preview` (§6.5) and the Phase 11 fast path. |
| `enabledPreviewProviders` | array | Enabled preview provider classes. Default: `MarkDown, TXT, OpenDocument, PNG, JPEG, GIF, BMP, XBitmap, Krita, WebP` — every other provider (`Movie`, `MP3`, `SVG`, `TIFF`, `PDF`, `HEIC`, `Imaginary`, office, …) is active **only when listed** (`PreviewManager::getEnabledDefaultProvider`). |
| `preview_imaginary_url` | string | Imaginary service base URL for native preview generation (Phase 11). **Sensitive — never log.** |
| `preview_imaginary_key` | string | Imaginary API key, sent as a query parameter. **Sensitive — never log.** |
| `preview_concurrency_new` | int | Max concurrent preview generations. Default: CPU count, fallback `4`. |
| `preview_concurrency_all` | int | Max concurrent preview requests (PHP-side outer gate). Default: 2× CPU count, fallback `8`. |
| `preview_format` | string | Output format override: `jpeg` (default) or `webp` (one-way override). |
| `preview_max_filesize_image` | int | Max source image size for generation, in MiB. Default `50`; `-1` = unlimited. |
| `preview_max_x`, `preview_max_y` | int | Max preview dimensions (the "max preview" size). Default `4096` each. |
| `preview_ffmpeg_path` | string | ffmpeg binary path (video previews; PHP falls back to a PATH search when unset). |
| `preview_libreoffice_path` | string | LibreOffice/OpenOffice binary path (office previews; PHP falls back to a PATH search when unset). |
| `serverid` | int | Snowflake server id for `oc_previews`/`oc_preview_locations` ids (§9.10). Default: `crc32(hostname)`. |

> **Preview quality** lives in `oc_appconfig` (§9.3), not `config.php`: appid `preview`, keys `jpeg_quality` / `webp_quality`, both default `80`.

---

---

Prev: [`17-logging-and-observability.md`](17-logging-and-observability.md) · Up: [`README.md`](README.md) · Next: [`19-compatibility-test-matrix.md`](19-compatibility-test-matrix.md)
