# Phase 0 — Foundation: DB, Migrations, Startup Caches

Goal: the binary connects to an existing Nextcloud DB or creates a fresh one, and all process-lifetime caches are populated before the first request is served.

---

### 0.1 Project scaffold
- [x] `cargo new nc-rust --bin`; add workspace `Cargo.toml` with feature flags `postgres`, `sqlite` that gate the corresponding `sqlx` driver (MySQL/MariaDB out of scope)
- [x] Add `axum`, `tokio` (full features), `sqlx` (runtime-tokio, macros, migrate), `serde`, `quick-xml`, `tracing`, `tracing-subscriber` to `Cargo.toml`
- [x] CI: `cargo build --all-features` passes with zero warnings (`RUSTFLAGS="-D warnings"`)
- [x] `#![forbid(unsafe_code)]` in `main.rs`; confirm `cargo clippy` clean

**Verify:** `cargo test --all-features` compiles and exits 0 with no tests yet. ✅

### 0.2 Config loading
- [x] Read `config/config.php` via a PHP-value parser (regex or dedicated crate) into a typed `NcConfig` struct
- [x] Support all keys listed in REQ §18 (`dbtype`, `dbhost`, `dbname`, `dbuser`, `dbpassword`, `dbtableprefix`, `datadirectory`, `maintenance`, `trusted_domains`, etc.)
- [x] Fall back to a TOML config file for fresh installs where no `config.php` exists
- [x] Missing required keys produce a descriptive startup error, not a panic

**Verify:** unit test: parse a synthetic `config.php` fixture, assert every field maps correctly to `NcConfig`. ✅ (`config::tests::parse_config_php`, `config::tests::defaults_applied_when_keys_absent`)

### 0.3 DB connection pool
- [x] Build `sqlx::Pool` at startup using the driver selected by `dbtype`
- [x] Pool min=5, max=50 (configurable); connection string constructed from `NcConfig`
- [x] Health-check query (`SELECT 1`) on startup; fatal error if DB unreachable
- [x] Table prefix (`oc_`) applied uniformly via a thin query-builder wrapper — no raw string `"oc_"` scattered through query files

**Verify:** integration test: start binary pointing at an empty SQLite file; confirm pool initialises.

### 0.4 SQL migrations
- [x] Write versioned migration files under `migrations/` for every table in REQ §9 (all `oc_*` tables required by core + files)
- [x] Migrations are additive-only: no `DROP COLUMN`, no `ALTER COLUMN` that changes type or nullability of existing columns
- [x] `sqlx::migrate!()` runs at startup before the HTTP listener opens
- [x] Migration applied to an already-migrated DB is a no-op (idempotent)

**Verify:**
- Apply migrations to an empty PostgreSQL DB; `\dt` lists all expected tables with correct column types.
- Apply migrations to a DB created by a stock PHP Nextcloud install; confirm `sqlx_migrations` table is created, no existing table is altered or dropped, and the server starts.

### 0.5 Startup: mime-type cache
- [x] `SELECT id, mimetype FROM oc_mimetypes` at startup → `Arc<RwLock<MimeCache>>`
- [x] Cache is populated before the HTTP listener opens
- [x] Write path: any insert into `oc_mimetypes` invalidates and rebuilds the cache atomically
- [x] Cache is readable under concurrent load with no mutex contention on reads (`RwLock` read guard, not write guard)

**Verify:** unit test: seed 10 mime types in test DB, start server, confirm cache contains all 10 without any DB query during a simulated `get_mime_id("image/jpeg")` call. ✅ (`mime::tests::lookup_by_name`, `mime::tests::lookup_by_id`, `mime::tests::concurrent_reads_do_not_block_each_other`)

### 0.6 Startup: app config cache
- [x] `SELECT appid, configkey, configvalue, type FROM oc_appconfig WHERE lazy = 0` at startup → `Arc<RwLock<AppConfigCache>>`
- [x] Typed access: `get_bool("core", "maintenance")`, `get_string("core", "version")`, etc.
- [x] Write path: any update to `oc_appconfig` for a non-lazy key invalidates the relevant entry atomically
- [x] `maintenance` flag is read from this cache on every request (used by maintenance-mode middleware)

**Verify:** unit test: insert `maintenance=1` into test DB, start server, call `config_cache.get_bool("core","maintenance")`, assert `true`. ✅ (`appconfig::tests::*` — 6 tests passing)
