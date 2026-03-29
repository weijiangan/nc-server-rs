-- Migration 0012: oc_preferences
-- Per-user configuration. Analogous to oc_appconfig but scoped to a user.

CREATE TABLE IF NOT EXISTS oc_preferences (
    userid      VARCHAR(64)  NOT NULL DEFAULT '',
    appid       VARCHAR(32)  NOT NULL DEFAULT '',
    configkey   VARCHAR(64)  NOT NULL DEFAULT '',
    configvalue TEXT,
    type        INTEGER      NOT NULL DEFAULT 1,
    lazy        SMALLINT     NOT NULL DEFAULT 0,
    flags       INTEGER      NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS preferences_userid_appid_key
    ON oc_preferences (userid, appid, configkey);
CREATE INDEX IF NOT EXISTS preferences_appid_key ON oc_preferences (appid, configkey);
CREATE INDEX IF NOT EXISTS preferences_userid    ON oc_preferences (userid);
