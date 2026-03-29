-- Migration 0014: oc_share
-- Shares between users, groups, links, and federated targets.
-- The Rust server reads this table to resolve permissions during DAV auth
-- and PROPFIND (oc:permissions, oc:share-permissions).
-- Write operations (create/delete share) remain with PHP-FPM files_sharing.

CREATE TABLE IF NOT EXISTS oc_share (
    id               BIGINT       NOT NULL PRIMARY KEY,
    share_type       SMALLINT     NOT NULL DEFAULT 0,
    share_with       VARCHAR(255),
    uid_owner        VARCHAR(64)  NOT NULL DEFAULT '',
    uid_initiator    VARCHAR(64),
    parent           BIGINT,
    item_type        VARCHAR(64)  NOT NULL DEFAULT '',
    item_source      VARCHAR(255),
    item_target      VARCHAR(255),
    file_source      BIGINT,
    file_target      VARCHAR(512),
    permissions      INTEGER      NOT NULL DEFAULT 0,
    stime            BIGINT       NOT NULL DEFAULT 0,
    accepted         SMALLINT     NOT NULL DEFAULT 0,
    expiration       DATETIME,
    token            VARCHAR(32),
    mail_send        SMALLINT     NOT NULL DEFAULT 0,
    note             TEXT         NOT NULL DEFAULT '',
    label            VARCHAR(255),
    attributes       TEXT,
    hide_download    SMALLINT     NOT NULL DEFAULT 0,
    password         VARCHAR(255),
    password_by_talk SMALLINT     NOT NULL DEFAULT 0
);

CREATE        INDEX IF NOT EXISTS share_item_source_type     ON oc_share (item_source, share_type);
CREATE        INDEX IF NOT EXISTS share_file_source_type     ON oc_share (file_source, share_type);
CREATE        INDEX IF NOT EXISTS share_token                ON oc_share (token);
CREATE        INDEX IF NOT EXISTS share_share_with           ON oc_share (share_with);
CREATE        INDEX IF NOT EXISTS share_parent               ON oc_share (parent);
CREATE        INDEX IF NOT EXISTS share_expiration           ON oc_share (expiration);
CREATE        INDEX IF NOT EXISTS share_uid_owner            ON oc_share (uid_owner);
CREATE        INDEX IF NOT EXISTS share_uid_initiator        ON oc_share (uid_initiator);
