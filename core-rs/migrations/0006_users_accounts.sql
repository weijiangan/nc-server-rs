-- Migration 0006: oc_users and oc_accounts
-- Core user identity tables.

CREATE TABLE IF NOT EXISTS oc_users (
    uid         VARCHAR(64)  NOT NULL PRIMARY KEY DEFAULT '',
    displayname VARCHAR(64),
    password    VARCHAR(255) NOT NULL DEFAULT '',
    uid_lower   VARCHAR(64)  NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS user_uid_lower ON oc_users (uid_lower);

-- oc_accounts: JSON blob of account properties per user.
CREATE TABLE IF NOT EXISTS oc_accounts (
    uid  VARCHAR(64) NOT NULL PRIMARY KEY DEFAULT '',
    data TEXT        NOT NULL DEFAULT ''
);

-- oc_accounts_data: indexed individual account properties.
CREATE TABLE IF NOT EXISTS oc_accounts_data (
    id       BIGINT      NOT NULL PRIMARY KEY,
    uid      VARCHAR(64) NOT NULL DEFAULT '',
    name     VARCHAR(64) NOT NULL DEFAULT '',
    value    TEXT,
    verified SMALLINT    NOT NULL DEFAULT 0
);

CREATE        INDEX IF NOT EXISTS accounts_data_uid      ON oc_accounts_data (uid);
CREATE        INDEX IF NOT EXISTS accounts_data_name     ON oc_accounts_data (name);
CREATE        INDEX IF NOT EXISTS accounts_data_value    ON oc_accounts_data (value);
