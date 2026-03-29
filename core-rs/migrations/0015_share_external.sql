-- Migration 0015: oc_share_external
-- Federated share mounts. Read by Rust when resolving external mount points.

CREATE TABLE IF NOT EXISTS oc_share_external (
    id           BIGINT        NOT NULL PRIMARY KEY,
    remote       VARCHAR(512)  NOT NULL,
    share_token  VARCHAR(64)   NOT NULL,
    password     VARCHAR(64),
    name         VARCHAR(64)   NOT NULL,
    owner        VARCHAR(64)   NOT NULL,
    user         VARCHAR(64)   NOT NULL,
    mountpoint   VARCHAR(4096) NOT NULL,
    mountpoint_hash VARCHAR(32) NOT NULL,
    remote_id    BIGINT        NOT NULL DEFAULT -1,
    accepted     INTEGER       NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS share_external_user   ON oc_share_external (user);
CREATE INDEX IF NOT EXISTS share_external_token  ON oc_share_external (share_token);
