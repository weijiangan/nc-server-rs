-- Migration 0013: oc_properties
-- DAV custom properties set via PROPPATCH. Keyed by userid + path + name.

CREATE TABLE IF NOT EXISTS oc_properties (
    id            INTEGER      NOT NULL PRIMARY KEY,
    userid        VARCHAR(64)  NOT NULL DEFAULT '',
    propertypath  VARCHAR(255) NOT NULL DEFAULT '',
    propertyname  VARCHAR(255) NOT NULL DEFAULT '',
    propertyvalue TEXT         NOT NULL DEFAULT '',
    valuetype     SMALLINT     NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS properties_userid   ON oc_properties (userid);
CREATE INDEX IF NOT EXISTS properties_path_uid ON oc_properties (userid, propertypath);
