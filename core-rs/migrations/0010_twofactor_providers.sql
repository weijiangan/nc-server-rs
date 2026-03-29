-- Migration 0010: oc_twofactor_providers
-- Read during DAV authentication to enforce 2FA status gate (REQ §4.5).
-- Written and managed by PHP-FPM 2FA apps; Rust only reads this table.

CREATE TABLE IF NOT EXISTS oc_twofactor_providers (
    provider_id VARCHAR(64)  NOT NULL DEFAULT '',
    uid         VARCHAR(64)  NOT NULL DEFAULT '',
    enabled     SMALLINT     NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS twofactor_providers_uid_provider
    ON oc_twofactor_providers (uid, provider_id);
