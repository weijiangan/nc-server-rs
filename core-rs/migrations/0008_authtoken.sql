-- Migration 0008: oc_authtoken
-- App tokens and session tokens. Looked up on every authenticated request
-- (token field holds SHA-512 of the bearer value).

CREATE TABLE IF NOT EXISTS oc_authtoken (
    id            BIGINT       NOT NULL PRIMARY KEY,
    uid           VARCHAR(64)  NOT NULL DEFAULT '',
    login_name    VARCHAR(255) NOT NULL DEFAULT '',
    password      VARCHAR(1024),
    name          VARCHAR(128) NOT NULL DEFAULT '',
    token         VARCHAR(200) NOT NULL DEFAULT '',
    type          SMALLINT     NOT NULL DEFAULT 0,
    remember      SMALLINT     NOT NULL DEFAULT 0,
    last_activity INTEGER      NOT NULL DEFAULT 0,
    last_check    INTEGER      NOT NULL DEFAULT 0,
    scope         VARCHAR(128) NOT NULL DEFAULT '{}',
    expires       INTEGER,
    private_key   TEXT,
    public_key    TEXT,
    version       SMALLINT     NOT NULL DEFAULT 1
);

CREATE UNIQUE INDEX IF NOT EXISTS authtoken_token     ON oc_authtoken (token);
CREATE        INDEX IF NOT EXISTS authtoken_uid       ON oc_authtoken (uid);
CREATE        INDEX IF NOT EXISTS authtoken_last_activity ON oc_authtoken (last_activity);
