//! App-token bootstrap for bearer-auth scenarios (2026-08-14).
//!
//! Every other scenario authenticates via Basic auth (the app token sent as
//! the password field), which exercises `basic.rs`'s lookup.  The
//! `Authorization: Bearer` path (`bearer.rs`) was never covered end-to-end —
//! which is how a PG-only decode bug (INT4 columns decoded as `i64`) 401'd
//! every Bearer request on the SUT while PHP returned 200, invisible to every
//! gate (SQLite's dynamic typing, the Basic-only harness, and the perf-gate's
//! statement counting).  A scenario with `auth: bearer` needs a real app
//! token per side, inserted exactly like PHP's `generateToken()` writes them.
//!
//! The insert mirrors nc-bench's `create_token` (phase 17.2): stored value =
//! `hex(sha512(raw + secret))` — PHP `PublicKeyTokenProvider::hashToken()` /
//! Rust `concat_hash` — `version = 2` (`PublicKeyTokenMapper` filters on it),
//! plus a real RSA-2048 keypair (PKCS#8 private / SPKI public PEM, exactly
//! what `openssl_pkey_export` writes).  Keyless rows crash PHP's
//! `updatePasswords()` on any plain-password login and broke the oracle for
//! every diff-test scenario (phase-17 record), so the keys are load-bearing.
//!
//! The row is created **before** the scenario's before-snapshot, so it is
//! present on both sides in both snapshots and produces no delta.  The raw
//! token is a fixed constant: the stored hash differs per instance (the
//! `secret` differs), and this is a dev stack whose admin credentials are
//! `admin:admin` anyway.

use anyhow::{Context, Result};
use sha2::Digest;

use crate::config::Instance;

/// The raw bearer token both sides authenticate with.
const RAW_TOKEN: &str = "nc-difftest-bearer-token";

/// Row name marking the harness's own token (cleaned + recreated per run).
const ROW_NAME: &str = "nc-difftest-bearer";

/// Ensure a valid v2 app token for `admin_user` exists on the instance,
/// returning the raw token the client authenticates with.
pub async fn ensure_bearer_token(inst: &Instance, admin_user: &str) -> Result<String> {
    let secret = read_secret(inst)?;

    // Stored value: hex(sha512(raw + secret)) — PHP `hashToken` / Rust `concat_hash`.
    let mut h = sha2::Sha512::new();
    h.update(RAW_TOKEN.as_bytes());
    h.update(secret.as_bytes());
    let stored = hex::encode(h.finalize());

    // RSA-2048 keypair, PEM-encoded exactly as PHP's openssl_pkey_export
    // writes them (PKCS#8 private, SPKI public).
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
    let mut rng = rand::rngs::OsRng;
    let priv_key = rsa::RsaPrivateKey::new(&mut rng, 2048).context("generating RSA-2048 keypair")?;
    let public_pem = priv_key
        .to_public_key()
        .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
        .context("exporting public key PEM")?;
    let private_pem = priv_key
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .context("exporting private key PEM")?
        .to_string();

    let pool = sqlx::PgPool::connect(&inst.dsn)
        .await
        .with_context(|| format!("connecting to {}", inst.dsn))?;

    // Drop the stale row from earlier runs, then insert fresh.
    sqlx::query("DELETE FROM oc_authtoken WHERE name = $1")
        .bind(ROW_NAME)
        .execute(&pool)
        .await
        .with_context(|| format!("cleaning stale bearer token in {}", inst.dsn))?;
    sqlx::query(
        "INSERT INTO oc_authtoken \
         (uid, login_name, name, token, type, remember, version, private_key, public_key) \
         VALUES ($1, $2, $3, $4, $5, 0, 2, $6, $7)",
    )
    .bind(admin_user)
    .bind(admin_user)
    .bind(ROW_NAME)
    .bind(&stored)
    .bind(1i16)
    .bind(&private_pem)
    .bind(&public_pem)
    .execute(&pool)
    .await
    .with_context(|| format!("inserting bearer token into {}", inst.dsn))?;
    Ok(RAW_TOKEN.to_string())
}

/// Read the instance's `secret` from `config/config.php` inside its container
/// (same `docker exec` mechanism as the harness's file-tree snapshots).
fn read_secret(inst: &Instance) -> Result<String> {
    let out = std::process::Command::new("docker")
        .args([
            "exec",
            &inst.container,
            "php",
            "-r",
            r#"require "/var/www/html/config/config.php"; echo $CONFIG["secret"];"#,
        ])
        .output()
        .context("running docker exec to read the instance secret")?;
    anyhow::ensure!(
        out.status.success(),
        "docker exec exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let secret = String::from_utf8(out.stdout).context("secret is not UTF-8")?;
    anyhow::ensure!(!secret.is_empty(), "empty secret in config.php");
    Ok(secret)
}
