//! App-token bootstrap for benchmark authentication (Phase 17).
//!
//! Real Nextcloud clients authenticate with **app tokens**, not plain
//! passwords: the token path is a cheap SHA-512 lookup (cached 5 min in
//! `nc-auth`'s token cache) instead of an argon2id verification (~120 ms on
//! this stack, measured against the stored hash).  Benchmarking with the
//! plain admin password would measure the KDF, not the server.
//!
//! Tokens are created by inserting a row into `oc_authtoken` directly (the
//! harness already has SQL access to both databases): the stored value is
//! `hex(sha512(raw + secret))` — exactly PHP's
//! `PublicKeyTokenProvider::hashToken()` (workspace/server/.../PublicKeyTokenProvider.php:414)
//! and Rust's `nc_auth::bearer::token_hash`/`concat_hash` (raw+secret
//! concatenation, SHA-512).  `version` is set to 2 because PHP's
//! `PublicKeyTokenMapper::getToken()` filters `WHERE token = ? AND version = ?`
//! against `PublicKeyToken::VERSION = 2`; the column default (1) would be
//! invisible to PHP.
//!
//! The OCS `core/apppassword` endpoint (the desktop-client flow) returns 405
//! on the dev oracle, so the SQL insert is the bootstrap of record.  If the
//! secret cannot be read or the insert fails, the plain admin password is
//! used with a warning (the numbers then include the argon2 tax on both
//! sides).
//!
//! The row must be a VALID `PublicKeyToken` (version 2): it carries a real
//! RSA keypair in `private_key`/`public_key`, like PHP's `generateToken()`
//! writes.  Without the keys, PHP's `updatePasswords()` (run on any
//! subsequent plain-password login) calls `encryptPassword($password, null)`
//! and 500s — observed during Phase 18 verification (Phase 17's keyless
//! inserts broke the oracle for every diff-test scenario).

use anyhow::{Context, Result};
use sha2::{Digest, Sha512};

use nc_difftest::config::Instance;

/// Clear the instance's bruteforce-throttle state before benchmarking.
///
/// Nextcloud (PHP and the faithful Rust port) imposes a
/// `100 ms × 2^attempts` sleep on EVERY request from a subnet with failed
/// logins in the last 12 h — one stray failed attempt (e.g. a typo'd curl
/// during debugging) silently adds ~200 ms to every measured request for
/// half a day.  A benchmark must measure from a clean slate; the dev stack's
/// attempts table is test bookkeeping (the differential harness already
/// treats it as volatile).  Failure is warned, not fatal: the numbers then
/// include the throttle sleep.
pub async fn reset_throttle(inst: &Instance) {
    let pool = match sqlx::PgPool::connect(&inst.dsn).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "  warn: cannot connect to {} to reset bruteforce state ({e})",
                inst.dsn
            );
            return;
        }
    };
    if let Err(e) = sqlx::query("DELETE FROM oc_bruteforce_attempts")
        .execute(&pool)
        .await
    {
        eprintln!(
            "  warn: failed to clear oc_bruteforce_attempts on {} ({e}) — \
             numbers may include the throttle sleep",
            inst.base_url
        );
    }
}

/// Create an app token for `inst` with the admin credentials, returning
/// `(login, token)`.  Falls back to `(admin_user, admin_pass)` with a warning.
pub async fn instance_creds(
    inst: &Instance,
    admin_user: &str,
    admin_pass: &str,
) -> Result<(String, String)> {
    match create_token(inst, admin_user).await {
        Ok(token) => {
            eprintln!(
                "  auth: app token created on {} ({}), hot path (no argon2)",
                inst.base_url, admin_user
            );
            Ok((admin_user.to_string(), token))
        }
        Err(e) => {
            eprintln!(
                "  warn: app-token bootstrap failed on {} ({e:#}) — \
                 falling back to plain-password auth (argon2-dominated numbers)",
                inst.base_url
            );
            Ok((admin_user.to_string(), admin_pass.to_string()))
        }
    }
}

/// Insert a fresh app token for `admin_user` into the instance's
/// `oc_authtoken`, returning the raw token the client authenticates with.
///
/// The row carries a real RSA keypair (like PHP `generateToken()`), and any
/// stale keyless `nc-bench` rows from earlier runs are deleted first — they
/// break PHP's `updatePasswords()`.
pub(crate) async fn create_token(inst: &Instance, admin_user: &str) -> Result<String> {
    let secret = read_secret(inst)?;

    // Raw token: 256 bits of randomness (two v4 UUIDs).
    let raw = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());

    // Stored value: hex(sha512(raw + secret)) — PHP `hashToken` / Rust `concat_hash`.
    let mut h = Sha512::new();
    h.update(raw.as_bytes());
    h.update(secret.as_bytes());
    let stored = hex::encode(h.finalize());

    // RSA-2048 keypair, PEM-encoded exactly as PHP's openssl_pkey_export
    // writes them (PKCS#8 private, SPKI public).  PHP's
    // `PublicKeyTokenProvider::encryptPassword()` needs a valid public key on
    // version-2 rows; keyless rows crash `updatePasswords()` with a TypeError.
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
    let mut rng = rand::rngs::OsRng;
    let priv_key = rsa::RsaPrivateKey::new(&mut rng, 2048)
        .context("generating RSA-2048 keypair for the app token")?;
    let public_pem = priv_key
        .to_public_key()
        .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
        .context("exporting public key PEM")?;
    // to_pkcs8_pem returns Zeroizing<String> — copy to a plain String for the
    // DB bind (PHP stores the PEM plain in oc_authtoken anyway).
    let private_pem = priv_key
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .context("exporting private key PEM")?
        .to_string();

    let pool = sqlx::PgPool::connect(&inst.dsn)
        .await
        .with_context(|| format!("connecting to {}", inst.dsn))?;

    // Drop stale keyless rows from earlier runs (they crash PHP's
    // updatePasswords on any plain-password login).
    sqlx::query("DELETE FROM oc_authtoken WHERE name = $1")
        .bind("nc-bench")
        .execute(&pool)
        .await
        .with_context(|| format!("cleaning stale nc-bench tokens in {}", inst.dsn))?;

    sqlx::query(
        "INSERT INTO oc_authtoken \
         (uid, login_name, name, token, type, remember, version, private_key, public_key) \
         VALUES ($1, $2, $3, $4, $5, 0, 2, $6, $7)",
    )
    .bind(admin_user)
    .bind(admin_user)
    .bind("nc-bench")
    .bind(&stored)
    .bind(1i16)
    .bind(&private_pem)
    .bind(&public_pem)
    .execute(&pool)
    .await
    .with_context(|| format!("inserting app token into {}", inst.dsn))?;
    Ok(raw)
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
