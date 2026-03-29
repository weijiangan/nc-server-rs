-- Migration 0004: oc_filecache_extended
-- Authoritative source for creation_time and upload_time (REQ §9.4).
-- Also holds metadata_etag, which the PHP FilesPlugin defines but never
-- wires to a PROPFIND handler — the Rust server returns it correctly.

CREATE TABLE IF NOT EXISTS oc_filecache_extended (
    fileid         BIGINT      NOT NULL PRIMARY KEY,
    metadata_etag  VARCHAR(40),
    creation_time  INTEGER     NOT NULL DEFAULT 0,
    upload_time    INTEGER     NOT NULL DEFAULT 0
);
