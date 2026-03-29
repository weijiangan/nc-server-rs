-- Migration 0005: oc_files_metadata
-- Per-file metadata store for {nc:}metadata-{key} DAV properties.

CREATE TABLE IF NOT EXISTS oc_files_metadata (
    id          BIGINT       NOT NULL PRIMARY KEY,
    file_id     BIGINT       NOT NULL,
    json        TEXT         NOT NULL DEFAULT '{}',
    sync_token  VARCHAR(15)  NOT NULL DEFAULT '',
    last_update DATETIME
);

CREATE UNIQUE INDEX IF NOT EXISTS files_metadata_file_id ON oc_files_metadata (file_id);
