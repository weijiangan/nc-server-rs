-- Migration 0016: oc_previews / oc_preview_locations / oc_preview_versions (REQ §9.10)
--
-- Preview metadata shared by the native (Rust) and PHP preview systems.
--
-- STATUS: like every migration here, this is DISABLED — `nc-db/src/migrate.rs`
-- does not run `sqlx::migrate!` because PHP owns the schema (improvements.md §I.3).
-- On a normal install PHP's Doctrine migrations already create these three tables
-- (Version33000Date20250819110529, …20251023110529, …20251023120529); this file is
-- ADDITIVE-ONLY (every statement is IF NOT EXISTS) so it is a no-op there and only
-- takes effect on a hypothetical Rust-only fresh install.
--
-- ids are client-side SNOWFLAKES — autoincrement was deliberately removed from all
-- three tables (Version33000Date20251023110529).  Never MAX(id)+1, never
-- INSERT … DEFAULT.  See nc-preview/src/snowflake.rs (PHP SnowflakeGenerator parity).

CREATE TABLE IF NOT EXISTS oc_previews (
    id                 BIGINT       NOT NULL PRIMARY KEY,
    file_id            BIGINT       NOT NULL,
    storage_id         BIGINT       NOT NULL,
    old_file_id        BIGINT,
    location_id        BIGINT,
    width              INTEGER      NOT NULL,
    height             INTEGER      NOT NULL,
    mimetype_id        INTEGER      NOT NULL,
    source_mimetype_id INTEGER      NOT NULL,
    max                SMALLINT     NOT NULL DEFAULT 0,
    cropped            SMALLINT     NOT NULL DEFAULT 0,
    encrypted          SMALLINT     NOT NULL DEFAULT 0,
    etag               VARCHAR(40)  NOT NULL DEFAULT '',
    mtime              INTEGER      NOT NULL DEFAULT 0,
    size               INTEGER      NOT NULL DEFAULT 0,
    version_id         BIGINT       NOT NULL DEFAULT -1
);

-- The unique key the generation path relies on for cross-writer (PHP ↔ Rust) race
-- resolution via INSERT … ON CONFLICT (file_id, width, height, mimetype_id, cropped,
-- version_id) DO NOTHING.  Its leading file_id column also serves the per-file lookups
-- (PHP additionally creates a standalone file_id index; redundant with this prefix).
CREATE UNIQUE INDEX IF NOT EXISTS previews_file_uniq_idx
    ON oc_previews (file_id, width, height, mimetype_id, cropped, version_id);

-- Object-store preview locations (out of scope for the local-disk fast path; present
-- for schema completeness so a subsequent PHP request finds the table it expects).
CREATE TABLE IF NOT EXISTS oc_preview_locations (
    id                BIGINT       NOT NULL PRIMARY KEY,
    bucket_name       VARCHAR(40)  NOT NULL,
    object_store_name VARCHAR(40)  NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS preview_locations_bucket_store
    ON oc_preview_locations (bucket_name, object_store_name);

-- Versioned-preview registry (local-disk files use version_id = -1 and write no row
-- here; present for schema completeness).
CREATE TABLE IF NOT EXISTS oc_preview_versions (
    id      BIGINT        NOT NULL PRIMARY KEY,
    file_id BIGINT        NOT NULL,
    version VARCHAR(1024) NOT NULL DEFAULT ''
);
