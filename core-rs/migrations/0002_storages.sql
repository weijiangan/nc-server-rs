-- Migration 0002: oc_storages
-- Each storage backend (local home, object store, external) has one row.
-- numeric_id is referenced by oc_filecache.storage.

CREATE TABLE IF NOT EXISTS oc_storages (
    numeric_id   BIGINT       NOT NULL PRIMARY KEY,
    id           VARCHAR(64)  NOT NULL DEFAULT '',
    available    SMALLINT     NOT NULL DEFAULT 1,
    last_checked INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS storages_id_index ON oc_storages (id);
