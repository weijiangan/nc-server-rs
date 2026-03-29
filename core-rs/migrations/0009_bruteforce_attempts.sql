-- Migration 0009: oc_bruteforce_attempts
-- One row per failed login attempt. Read and written by the brute-force
-- throttling middleware on every failed auth.

CREATE TABLE IF NOT EXISTS oc_bruteforce_attempts (
    id       BIGINT       NOT NULL PRIMARY KEY,
    action   VARCHAR(64)  NOT NULL DEFAULT '',
    occurred INTEGER      NOT NULL DEFAULT 0,
    ip       VARCHAR(255) NOT NULL DEFAULT '',
    subnet   VARCHAR(255) NOT NULL DEFAULT '',
    metadata VARCHAR(255) NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS bruteforce_attempts_ip     ON oc_bruteforce_attempts (ip);
CREATE INDEX IF NOT EXISTS bruteforce_attempts_subnet ON oc_bruteforce_attempts (subnet);
CREATE INDEX IF NOT EXISTS bruteforce_attempts_action ON oc_bruteforce_attempts (action, ip, occurred);
