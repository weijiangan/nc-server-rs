# Deferred Improvements

Nice-to-have items that would improve correctness or operator experience but are not required for API compatibility. Not blocking any phase completion gate.

---

## I.1 Hot-reload `config.php` for maintenance mode

**Context:** PHP re-reads `config.php` on every request (shared-nothing bootstrap), so `occ maintenance:mode --on` takes effect immediately. The Rust server parses `config.php` once at startup — toggling maintenance while running requires a restart.

**Fix:** Watch `config.php` with the `notify` crate (filesystem event) or poll it every ~5 seconds. On change, re-parse and update `AppState::nc_config`. Maintenance flag and any other `NcConfig` fields (e.g. `trusted_domains`, `installed`) would then reflect changes without restart.

**Scope:** `nc-db/src/config.rs` (re-parse), `nc-server/src/main.rs` (spawn watcher task), `AppState` (wrap `nc_config` in `Arc<RwLock<NcConfig>>`).

**Caveat:** `Arc<RwLock<NcConfig>>` adds a read-lock on every `nc_config` access. Profile first — if the hot path (DAV PROPFIND) shows lock contention, use `arc-swap` instead for lock-free reads.

---

## I.2 Mime cache staleness after `occ files:scan`

**Context:** `occ files:scan` may INSERT new rows into `oc_mimetypes` when it encounters file types it has never indexed before. Rust's `refresh_mime_cache()` is only called after Rust's own inserts — it is not triggered by PHP-side inserts. Until the Rust server is restarted, PROPFIND responses for files of the new mime type will show `application/octet-stream` instead of the correct type.

**Impact:** cosmetic only — wrong `{DAV:}getcontenttype` value for the new type. No data corruption. Affects only file types first introduced by a PHP-side scan, which in practice is rare once the library is stable.

**Fix:** periodically re-query `oc_mimetypes` and call `refresh_mime_cache()` if the row count has changed (cheap `SELECT COUNT(*)` check every 60 seconds), or hook into the same `notify`-based `config.php` watcher from §I.1.

---

## I.3 Nextcloud upgrade (`occ upgrade`) compatibility

**Context:** `occ upgrade` runs PHP's Doctrine-based migration system, which tracks applied migrations in `oc_migrations`. The Rust server uses `sqlx::migrate!()` which tracks its own migrations in `_sqlx_migrations`. These two tables are independent. If a PHP upgrade alters a table that Rust also has a migration for, the schema may diverge silently or `sqlx migrate!()` may fail on the next Rust restart.

**Risk:** high — silent schema corruption is possible if `sqlx migrate!()` is run against a DB that PHP has already migrated beyond the Rust migration's assumptions.

**Fix options:**
1. **Remove Rust migrations entirely** and replace `migrate!()` at startup with a schema validation check (verify expected tables and critical columns exist, bail with a clear error if not). Let PHP own the schema completely. This is the correct long-term approach given "PHP installs, Rust serves."
2. **Guard `sqlx migrate!()` with a PHP schema version check**: read `oc_appconfig WHERE appid='core' AND configkey='dbupdates'`; if PHP has applied migrations beyond what Rust knows about, skip Rust migrations and warn.

Option 1 is recommended. Tracked as a future task because it requires rewriting Phase 0.4 and removing migration files.

---

## I.4 Token revocation window

**Context:** The auth hot cache (`nc-auth/src/token.rs`) has a 5-minute TTL (`TOKEN_TTL = 5 * 60s`). When a user logs out via the web UI, PHP deletes the `oc_authtoken` row immediately. For up to 5 minutes after logout, the Rust server still accepts that token from a cached hit.

**Impact:** bearer token sessions remain valid for up to 5 minutes after PHP-side logout. This is a security gap for shared/managed devices.

**Fix:** reduce `TOKEN_TTL` (trade-off: more DB queries per request) or implement an explicit eviction signal. The cleanest approach is a lightweight invalidation endpoint — PHP calls `POST /__nc_internal/token_evict` (Unix-socket-only, internal) when it deletes a token. Rust removes the cache entry immediately. Alternatively, subscribe to PostgreSQL `NOTIFY` on `oc_authtoken` deletes.

---

## I.5 App enable/disable without restart

**Context:** The route registry (`nc-fastcgi::build_route_registry`) is built once at startup from `apps/*/appinfo/routes.php`. `occ app:enable` or `occ app:disable` while `nc-server` is running does not update the registry — new app routes return `404` and disabled app routes still forward to PHP-FPM (which will error).

**Impact:** operators must restart `nc-server` after any app enable/disable operation.

**Fix:** watch the `apps/` directory for `appinfo/routes.php` changes (via `notify`) or poll `oc_appconfig WHERE configkey='enabled'` periodically and rebuild the registry on change. The 30-second capability refresh task in Phase 7.7 is a natural trigger point.

---

## I.6 `/cron.php` not routed

**Context:** `cron.php` is not registered in `router.rs` and currently returns `404`. Nextcloud's background job system calls `/cron.php` via HTTP (system cron or Ajax cron). If the Rust server is the sole listener, cron jobs stop running.

**Impact:** all background jobs fail silently — expiry cleanup, activity digests, federation sync, etc.

**Fix:** add `/cron.php` to the PHP-FPM fallback routes in `router.rs`. One line: `.route("/cron.php", axum::routing::any(php_fpm_fallback))`. No auth injection needed — `cron.php` validates its own execution context internally.

---

## I.7 Split log streams

**Context:** PHP logs to `nextcloud.log` (or syslog) via `\OC\Log`. The Rust server logs to stdout via `tracing`. Operators see two separate log streams with no shared request ID.

**Impact:** correlating a PHP-side error with the Rust-side request that triggered it requires manual timestamp matching.

**Fix:** inject the Rust-generated `X-Request-Id` as a FastCGI param (`HTTP_X_REQUEST_ID`) so PHP can include it in its own log lines. PHP's `\OC\Log` formatter would need a patch to read and forward this header — or the PHP shim logs it explicitly before calling `OC::handleRequest()`.

---

## I.8 Secret rotation requires restart

**Context:** `config.php`'s `secret` key is read at startup into `NcConfig` and used to hash all bearer tokens (`SHA-512(token || secret)`). If `secret` is rotated (e.g. via `occ config:system:set secret`), the Rust server continues using the old secret. All token lookups fail until restart — but cached tokens in the hot cache also remain valid under the old secret for up to 5 minutes (§I.4).

**Impact:** secret rotation is a disruptive operation — requires coordinated restart. Also overlaps with §I.1 (config.php hot-reload) since `secret` is a system config key.

**Fix:** §I.1's config.php watcher covers this automatically — on secret change, re-parse `NcConfig`, flush the entire token hot cache (since all cached hashes computed under the old secret are now invalid).
