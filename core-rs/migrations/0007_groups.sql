-- Migration 0007: oc_groups and oc_group_user

CREATE TABLE IF NOT EXISTS oc_groups (
    gid VARCHAR(255) NOT NULL PRIMARY KEY DEFAULT ''
);

CREATE TABLE IF NOT EXISTS oc_group_user (
    gid VARCHAR(255) NOT NULL DEFAULT '',
    uid VARCHAR(64)  NOT NULL DEFAULT ''
);

CREATE UNIQUE INDEX IF NOT EXISTS gu_gid_uid  ON oc_group_user (gid, uid);
CREATE        INDEX IF NOT EXISTS gu_uid      ON oc_group_user (uid);
