-- Migration 0001: oc_mimetypes
-- Required by oc_filecache (FK on mimetype/mimepart columns).
-- Must be created before oc_filecache.

CREATE TABLE IF NOT EXISTS oc_mimetypes (
    id       BIGINT      NOT NULL PRIMARY KEY,
    mimetype VARCHAR(255) NOT NULL DEFAULT ''
);

CREATE UNIQUE INDEX IF NOT EXISTS mimetype_id_index ON oc_mimetypes (mimetype);
