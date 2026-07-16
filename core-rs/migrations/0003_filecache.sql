-- Migration 0003: oc_filecache
-- Central file metadata table. Every node (file or directory) in every
-- storage has exactly one row here.
--
-- Notes on intentional omissions:
--   - Foreign key constraints on storage/mimetype/mimepart are declared
--     but SQLite ignores them unless PRAGMA foreign_keys = ON. The Rust
--     code enforces referential integrity at the application layer.
--   - path is VARCHAR(4000) but SQLite stores it as TEXT; PostgreSQL and
--     MySQL honour the length limit.

CREATE TABLE IF NOT EXISTS oc_filecache (
    fileid           INTEGER      NOT NULL PRIMARY KEY,
    storage          BIGINT       NOT NULL DEFAULT 0,
    path             VARCHAR(4000),
    path_hash        VARCHAR(32)  NOT NULL DEFAULT '',
    parent           BIGINT       NOT NULL DEFAULT 0,
    name             VARCHAR(250),
    mimetype         BIGINT       NOT NULL DEFAULT 0,
    mimepart         BIGINT       NOT NULL DEFAULT 0,
    size             BIGINT       NOT NULL DEFAULT 0,
    mtime            INTEGER      NOT NULL DEFAULT 0,
    storage_mtime    INTEGER      NOT NULL DEFAULT 0,
    encrypted        SMALLINT     NOT NULL DEFAULT 0,
    unencrypted_size BIGINT       NOT NULL DEFAULT 0,
    etag             VARCHAR(40),
    permissions      INTEGER                DEFAULT 0,
    checksum         VARCHAR(255),
    creation_time    INTEGER      NOT NULL DEFAULT 0,
    upload_time      INTEGER      NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS fs_storage_path_hash ON oc_filecache (storage, path_hash);
CREATE        INDEX IF NOT EXISTS fs_parent_name       ON oc_filecache (parent, name);
CREATE        INDEX IF NOT EXISTS fs_storage_mimetype  ON oc_filecache (storage, mimetype);
CREATE        INDEX IF NOT EXISTS fs_storage_mimepart  ON oc_filecache (storage, mimepart);
CREATE        INDEX IF NOT EXISTS fs_storage_size      ON oc_filecache (storage, size);
CREATE        INDEX IF NOT EXISTS fs_mtime             ON oc_filecache (mtime);
