## 9. Database Schema

The Rust server manages the following tables (minimum required for core + files). All table names use the `oc_` prefix by default (configurable via `dbtableprefix`).

### 9.1 Users and accounts

**`oc_users`**
- `uid` VARCHAR(64) PK
- `displayname` VARCHAR(64)
- `password` VARCHAR(255) — hashed (bcrypt)
- `uid_lower` VARCHAR(64)

**`oc_accounts`**
- `uid` VARCHAR(64) PK
- `data` LONGTEXT (JSON blob of account properties)

**`oc_accounts_data`**
- `id` BIGINT PK AI
- `uid` VARCHAR(64)
- `name` VARCHAR(64)
- `value` MEDIUMTEXT
- `verified` SMALLINT

**`oc_groups`**
- `gid` VARCHAR(255) PK

**`oc_group_user`**
- `gid` VARCHAR(255)
- `uid` VARCHAR(64)

### 9.2 Auth tokens

**`oc_authtoken`**
- `id` BIGINT PK AI
- `uid` VARCHAR(64) NOT NULL
- `login_name` VARCHAR(255) NOT NULL
- `password` VARCHAR(1024) — encrypted password (for token refresh)
- `name` VARCHAR(128) NOT NULL — device label
- `token` VARCHAR(200) NOT NULL UNIQUE — SHA-512 of token value
- `type` SMALLINT — 0=temporary, 1=permanent, 2=wipe
- `remember` SMALLINT — whether to persist session
- `last_activity` INT — unix ts
- `last_check` INT — unix ts
- `scope` VARCHAR(128) — JSON lockdown scope
- `expires` INT — optional expiry
- `private_key` TEXT
- `public_key` TEXT
- `version` SMALLINT

**`oc_bruteforce_attempts`**
- `id` BIGINT PK AI
- `action` VARCHAR(64)
- `occurred` INT
- `ip` VARCHAR(255)
- `subnet` VARCHAR(255)
- `metadata` VARCHAR(255) — JSON

### 9.3 App config and user preferences

**`oc_appconfig`**
- `appid` VARCHAR(32)
- `configkey` VARCHAR(64)
- `configvalue` CLOB/TEXT
- `type` INT (1=string, 2=int, 4=float, 8=bool, 16=array)
- `lazy` SMALLINT

**`oc_preferences`**
- `userid` VARCHAR(64)
- `appid` VARCHAR(32)
- `configkey` VARCHAR(64)
- `configvalue` CLOB/TEXT
- `type` INT
- `lazy` SMALLINT
- `flags` INT

### 9.4 File storage and cache

**`oc_storages`**
- `numeric_id` BIGINT PK AI
- `id` VARCHAR(64) UNIQUE — e.g. `home::alice`, `object::store::s3::…`
- `available` SMALLINT
- `last_checked` INT

**`oc_filecache`**
- `fileid` BIGINT PK AI
- `storage` BIGINT FK `oc_storages.numeric_id`
- `path` VARCHAR(4000)
- `path_hash` VARCHAR(32) — md5 of path; unique with storage
- `parent` BIGINT FK self
- `name` VARCHAR(250)
- `mimetype` BIGINT FK `oc_mimetypes.id`
- `mimepart` BIGINT FK `oc_mimetypes.id`
- `size` BIGINT — -1 = unscanned
- `mtime` INT
- `storage_mtime` INT
- `encrypted` SMALLINT
- `unencrypted_size` BIGINT
- `etag` VARCHAR(40)
- `permissions` INT — CRUDS bitmask
- `checksum` VARCHAR(255)

> **Note:** `creation_time` and `upload_time` are **not** columns of `oc_filecache`. They live exclusively in `oc_filecache_extended` (added in NC 17 via `Version17000Date20190514105811`). Do not SELECT them from `oc_filecache`.

**`oc_mimetypes`**
- `id` BIGINT PK AI
- `mimetype` VARCHAR(255) UNIQUE

**`oc_filecache_extended`**
- `fileid` BIGINT PK FK `oc_filecache.fileid`
- `metadata_etag` VARCHAR(40)
- `creation_time` INT — authoritative source for `{nc:}creation_time`; this is the **only** table that has this column
- `upload_time` INT — authoritative source for `{nc:}upload_time`; this is the **only** table that has this column

**`oc_files_trash`**
- `auto_id` BIGINT PK AI
- `id` BIGINT NOT NULL — `oc_filecache.fileid` of the trashed node
- `user` VARCHAR(64) NOT NULL — UID of the user who deleted the file
- `timestamp` INT NOT NULL — Unix timestamp of deletion
- `location` VARCHAR(512) NOT NULL — original path before deletion (e.g. `files/Documents/report.pdf`)
- `type` VARCHAR(8) — `'file'` or `'folder'`
- `deleted_by` VARCHAR(64) — UID of the user who performed the deletion (same as `user` for direct deletes)

**`oc_files_metadata`**
- `id` BIGINT PK AI
- `file_id` BIGINT
- `json` LONGTEXT
- `sync_token` VARCHAR(15)
- `last_update` DATETIME

### 9.5 DAV properties

**`oc_properties`**
- `id` BIGINT PK AI
- `userid` VARCHAR(64)
- `propertypath` VARCHAR(255)
- `propertyname` VARCHAR(255)
- `propertyvalue` MEDIUMTEXT
- `valuetype` SMALLINT

### 9.6 Shares

**`oc_share`**
- `id` BIGINT PK AI
- `share_type` SMALLINT
- `share_with` VARCHAR(255)
- `uid_owner` VARCHAR(64)
- `uid_initiator` VARCHAR(64)
- `parent` BIGINT
- `item_type` VARCHAR(64)
- `item_source` VARCHAR(255)
- `item_target` VARCHAR(255)
- `file_source` BIGINT
- `file_target` VARCHAR(512)
- `permissions` INT
- `stime` BIGINT
- `accepted` SMALLINT
- `expiration` DATETIME
- `token` VARCHAR(32)
- `mail_send` SMALLINT
- `note` MEDIUMTEXT
- `label` VARCHAR(255)
- `attributes` MEDIUMTEXT
- `hide_download` SMALLINT
- `password` VARCHAR(255)
- `password_by_talk` SMALLINT

**`oc_share_external`** (federated shares — queried by PHP-FPM app)

### 9.8 Two-factor auth (required for DAV auth enforcement)

**`oc_twofactor_providers`**
- `provider_id` VARCHAR(64)
- `uid` VARCHAR(64)
- `enabled` SMALLINT

This table is read during DAV authentication (§4.5) to check if the user has a pending 2FA challenge. The Rust server must query it even though the 2FA apps themselves are managed by PHP-FPM.

### 9.7 Migration strategy

- Use `sqlx::migrate!()` with versioned SQL files (one file per schema change, named with timestamp).
- Migrations are idempotent: check `sqlx_migrations` table (created automatically by sqlx) for applied versions.
- Interop with PHP Nextcloud databases: SQL migration files must be additive only when the schema already exists. Never drop or rename existing columns.
- On fresh install: create all tables from scratch, then:
  - Write to **`config.php`** (via `SystemConfig::setValue`): `installed = true`, `instanceid`, `secret`, `passwordsalt`
  - Write to **`oc_appconfig`**: `core / oc_version = {version}`, `core / versionstring = {versionstring}`, `core / lastupdatedat = {timestamp}`, `core / installedat = {microtime}`
  - An admin user record in `oc_users` and `oc_accounts`
- Supported DBs: PostgreSQL, MySQL/MariaDB, SQLite.

---

---

Prev: [`08-files-app-rest-endpoints.md`](08-files-app-rest-endpoints.md) · Up: [`README.md`](README.md) · Next: [`10-php-fpm-integration.md`](10-php-fpm-integration.md)
