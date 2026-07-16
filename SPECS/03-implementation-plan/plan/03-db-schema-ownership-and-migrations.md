## 0) DB schema ownership and migrations

The Rust server must be able to both connect to an existing Nextcloud DB and create one from scratch for fresh installs. No PHP is involved in either case.

### Coding steps
1. Extract the minimal Nextcloud DB schema needed for core+files into versioned SQL migration files:
   - `oc_storages`, `oc_filecache`, `oc_filecache_extended` (authoritative `creation_time`/`upload_time`)
   - `oc_mimetypes` (required by `oc_filecache` foreign keys)
   - `oc_files_metadata` (per-file metadata, `{nc:}metadata-{key}` properties)
   - `oc_accounts`, `oc_users`, `oc_groups`, `oc_group_user`
   - `oc_authtoken`, `oc_bruteforce_attempts`
   - `oc_twofactor_providers` (read during DAV auth for 2FA enforcement)
   - `oc_share`, `oc_share_external`
   - `oc_properties` (DAV custom properties)
   - `oc_appconfig`, `oc_preferences`
2. Use `sqlx::migrate!()` to apply migrations at startup — creates the schema if absent, no-ops if already up to date, applies deltas otherwise.
3. Support PostgreSQL and SQLite; driver selected via connection string in config. MySQL/MariaDB is out of scope for this implementation.
4. For existing Nextcloud installs: connect, run `migrate!()` — it detects the current schema version and skips already-applied migrations.
5. For fresh installs: `migrate!()` creates all tables from scratch, then the Rust setup flow writes initial `oc_appconfig` values (version, installed flag, etc.).
6. At startup, populate the process-lifetime in-memory caches that are owned by the DB layer:
   - Mime-type map: `SELECT id, mimetype FROM oc_mimetypes` → `Arc<RwLock<HashMap<String, i64>>>`. Invalidated only on write (mime types change rarely). Eliminates a per-request join on every PROPFIND row.
   - App config hot values: `SELECT appid, configkey, configvalue FROM oc_appconfig WHERE lazy = 0` → `Arc<RwLock<HashMap>>`. Invalidated on any config write. Covers `maintenance`, `data-fingerprint`, forbidden filename lists, quota defaults.

### Verification steps
- Unit-test each migration file: apply to empty DB, verify expected tables/columns exist.
- Smoke-test against a PHP-created Nextcloud DB: run migrations, confirm no destructive changes, run the Behat/Gherkin suite normally.
- Smoke-test a fresh DB: run migrations from scratch, confirm the server starts and passes `build/integration/features/maintenance-mode.feature` and `/status.php` checks.

---

---

Prev: [`02-key-dependencies.md`](02-key-dependencies.md) · Up: [`README.md`](README.md) · Next: [`04-stand-up-the-http-server-skeleton.md`](04-stand-up-the-http-server-skeleton.md)
