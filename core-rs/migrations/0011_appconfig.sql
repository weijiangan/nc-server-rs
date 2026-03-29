-- Migration 0011: oc_appconfig
-- Global application configuration. Non-lazy rows are cached in memory
-- at startup (AppConfigCache). Lazy rows are read on demand.

CREATE TABLE IF NOT EXISTS oc_appconfig (
    appid       VARCHAR(32)  NOT NULL DEFAULT '',
    configkey   VARCHAR(64)  NOT NULL DEFAULT '',
    configvalue TEXT,
    type        INTEGER      NOT NULL DEFAULT 1,
    lazy        SMALLINT     NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS appconfig_appid_key ON oc_appconfig (appid, configkey);
CREATE        INDEX IF NOT EXISTS appconfig_lazy      ON oc_appconfig (lazy);
CREATE        INDEX IF NOT EXISTS appconfig_appid     ON oc_appconfig (appid);
