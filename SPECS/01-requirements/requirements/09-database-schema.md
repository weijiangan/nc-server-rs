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
- `fileid` BIGINT PK, auto-increment — **allocated by the DB sequence** (Postgres/MySQL). Because this table is shared with PHP-FPM inserts (versions, trash restore, `occ`), Rust must let the DB assign it (`INSERT … DEFAULT … RETURNING`), **never** hand-pick `MAX(fileid)+1` (that doesn't advance the Postgres sequence and causes duplicate-key collisions with later PHP inserts)
- `storage` BIGINT FK `oc_storages.numeric_id`
- `path` VARCHAR(4000)
- `path_hash` VARCHAR(32) — md5 of path; unique with storage
- `parent` BIGINT FK self
- `name` VARCHAR(250)
- `mimetype` BIGINT FK `oc_mimetypes.id`
- `mimepart` BIGINT FK `oc_mimetypes.id` — id of the **type part** (the substring before `/`: `image` for `image/png`, `httpd` for `httpd/unix-directory`), stored **without** a trailing slash. Type-filter queries use `WHERE mimepart = {id}`, so it must match PHP's `getId(substr(mimetype, 0, strpos('/')))`
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

**`oc_files_trash`** (PHP migration `apps/files_trashbin/lib/Migration/Version1010Date20200630192639.php`)
- `auto_id` BIGINT PK AI
- `id` VARCHAR(250) NOT NULL — original **basename** of the trashed node (PHP stores the filename, **not** a fileid)
- `user` VARCHAR(64) NOT NULL — UID of the user who owns the trashbin
- `timestamp` VARCHAR(12) NOT NULL — Unix timestamp of deletion, stored as a **string**
- `location` VARCHAR(512) NOT NULL — original parent directory relative to `files/` (PHP `pathinfo()['dirname']`, e.g. `Documents`; `.` for root-level items)
- `type` VARCHAR(4) — nullable; `move2trash()` does **not** set it (left `NULL`)
- `mime` VARCHAR(255) — nullable; also not set by `move2trash()`
- `deleted_by` VARCHAR(64) — UID of the user who performed the deletion (added by `Version1020Date20240403003535`; same as `user` for direct deletes)
- Indexes: PK `auto_id`; `id_index(id)`, `timestamp_index(timestamp)`, `user_index(user)`

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

### 9.9 Favorites / personal tags (files object tagging)

These tables back the `{oc:}favorite` and `{oc:}tags` DAV properties (§6.5.1), the star/unstar PROPPATCH, and the `favorite` rule of the `filter-files` REPORT (§6.10). In PHP they are the `ITags`/`ITagManager` store (`OC\Tagging`), used by `apps/dav/.../TagsPlugin.php`. Because these are read on every web PROPFIND and written on the Rust-native files tree, Rust owns them.

**`oc_vcategory`**
- `id` BIGINT PK AI
- `uid` VARCHAR(64) — owner user
- `type` VARCHAR(64) — object type; `'files'` for file tags/favorites
- `category` VARCHAR(255) — tag name; the favorite tag is the literal `_$!<Favorite>!$_`

**`oc_vcategory_to_object`**
- `objectid` BIGINT — `oc_filecache.fileid`
- `categoryid` BIGINT — FK `oc_vcategory.id`
- `type` VARCHAR(64) — `'files'`
- PK (`objectid`, `categoryid`, `type`)

> The favorite flag is not a filecache column — it is the presence of a `(fileid, favorite-category)` row here. Deleting/moving a file to trash keeps the `fileid`, so favorites survive a trash round-trip; permanent delete (PHP-FPM) removes the mapping.

### 9.8 Two-factor auth (required for DAV auth enforcement)

**`oc_twofactor_providers`**
- `provider_id` VARCHAR(64)
- `uid` VARCHAR(64)
- `enabled` SMALLINT

This table is read during DAV authentication (§4.5) to check if the user has a pending 2FA challenge. The Rust server must query it even though the 2FA apps themselves are managed by PHP-FPM.

### 9.10 Previews (shared with PHP-FPM)

Created by the PHP Doctrine migrations `core/Migrations/Version33000Date20250819110529.php` (tables), `Version33000Date20251023110529.php` (autoincrement **removed** from all three `id` columns — ids are **client-side snowflakes** via `ISnowflakeGenerator` (`lib/private/Snowflake/SnowflakeGenerator.php`), never a DB sequence), and `Version33000Date20251023120529.php` (unique index). The Phase 11 preview fast path reads and writes these tables and must stay PHP-compatible in both directions; Rust migrations create them only on fresh installs (additive-only no-op otherwise, §9.7).

**`oc_previews`**
- `id` BIGINT UNSIGNED PK — snowflake-assigned (`SnowflakeAwareEntity`)
- `file_id` BIGINT UNSIGNED NOT NULL — `oc_filecache.fileid`; indexed
- `storage_id` BIGINT UNSIGNED NOT NULL — `oc_storages.numeric_id`
- `old_file_id` BIGINT UNSIGNED NULL — legacy filecache id (set only by the legacy-layout migration)
- `location_id` BIGINT UNSIGNED NULL — → `oc_preview_locations.id` (object store only; NULL on local disk)
- `width` INTEGER UNSIGNED NOT NULL
- `height` INTEGER UNSIGNED NOT NULL
- `mimetype_id` INTEGER NOT NULL — **output** image mimetype (`oc_mimetypes.id`)
- `source_mimetype_id` INTEGER NOT NULL — source file mimetype
- `max` BOOLEAN NOT NULL DEFAULT false — the single max preview per file/version
- `cropped` BOOLEAN NOT NULL DEFAULT false
- `encrypted` BOOLEAN NOT NULL DEFAULT false
- `etag` CHAR(40) NOT NULL — the **source file's etag at generation time** (served as the preview response `ETag`)
- `mtime` INTEGER UNSIGNED NOT NULL — **generation timestamp** (not the file's mtime; used only for TTL expiry cleanup)
- `size` INTEGER UNSIGNED NOT NULL — byte size
- `version_id` BIGINT NOT NULL DEFAULT -1 — → `oc_preview_versions.id`; `-1` = un-versioned (local disk)
- Indexes: PK `id`; `(file_id)`; UNIQUE `previews_file_uniq_idx (file_id, width, height, mimetype_id, cropped, version_id)`

Preview bytes live at `{datadirectory}/appdata_{instanceid}/preview/{md5(file_id)[0..7], each char a nested dir}/{file_id}/{version-}{w}-{h}[-crop][-max].{ext}` on local disk (PHP `LocalPreviewStorage::constructPath`). Staleness is maintained by deleting a file's rows on content write (PHP `Preview\Watcher`), never by comparing `mtime`/`etag` at read.

**`oc_preview_locations`** (object store only)
- `id` BIGINT UNSIGNED PK — snowflake-assigned
- `bucket_name` VARCHAR(40) NOT NULL
- `object_store_name` VARCHAR(40) NOT NULL
- UNIQUE `(bucket_name, object_store_name)`

**`oc_preview_versions`** (versioned/object-store files only)
- `id` BIGINT UNSIGNED PK — the same snowflake as the preview row it belongs to
- `file_id` BIGINT UNSIGNED NOT NULL
- `version` VARCHAR(1024) NOT NULL DEFAULT ''

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
