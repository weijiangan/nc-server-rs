//! PHP-compatible password-hash verification (parity with
//! `OC\Security\Hasher::verify`).
//!
//! Nextcloud stores `oc_users.password` as a **version-prefixed** PHC string:
//! `<version>|<hash>`, where the version selects the algorithm family:
//!
//! | version | algorithm | hash prefix |
//! |---------|-----------|-------------|
//! | `1`     | bcrypt    | `$2y$`      |
//! | `2`     | argon2i   | `$argon2i$` |
//! | `3`     | argon2id  | `$argon2id$`|
//!
//! Modern installs default to `3|$argon2id$…` (PHP's `password_hash` with
//! `PASSWORD_ARGON2ID`).  The version prefix is Nextcloud's own framing; the
//! `<hash>` body is a standard PHC/crypt string that PHP's `password_verify()`
//! dispatches purely on its `$…$` identifier.
//!
//! Hashes **without** a numeric prefix are legacy (pre-versioning ownCloud):
//! 60-char PHPass/bcrypt (`password_verify($message . $salt, …)`) or 40-char
//! SHA-1 (`hash_equals($hash, sha1($message))`).
//!
//! Reference: `workspace/server/lib/private/Security/Hasher.php` —
//! `verify()` → `splitHash()` → `verifyHash()` / `legacyHashVerify()`.

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use sha1::{Digest, Sha1};

/// Verify `message` against a stored `oc_users.password` value.
///
/// `legacy_salt` is the `passwordsalt` system-config value, used **only** by
/// the legacy (unprefixed) PHPass path.  It is irrelevant for every
/// version-prefixed hash, which is all a modern install contains.
///
/// Returns `true` iff the message matches.  This mirrors PHP's
/// `Hasher::verify()` return value; the `$newHash` rehash out-parameter is
/// intentionally not modelled (see module docs of the caller — rehash-on-login
/// is a write side effect that is never triggered while the stored algorithm
/// is already the preferred one, which is the case for argon2id installs).
pub fn verify_password(message: &str, prefixed_hash: &str, legacy_salt: &str) -> bool {
    match split_hash(prefixed_hash) {
        // Versioned hash: dispatch on the PHC identifier, exactly as PHP's
        // `password_verify()` does inside `verifyHash()` for versions 1/2/3.
        Some((version, hash)) if (1..=3).contains(&version) => verify_hash(message, hash),
        // Unrecognized version → PHP's `switch` falls through and returns false.
        Some(_) => false,
        // No numeric prefix → legacy PHPass / SHA-1 path.
        None => legacy_hash_verify(message, prefixed_hash, legacy_salt),
    }
}

/// Split a `<version>|<hash>` prefix (PHP `Hasher::splitHash`).
///
/// Returns `Some((version, hash))` only when the part before the **first** `|`
/// is a positive integer; otherwise `None` (treated as a legacy unprefixed
/// hash).  Matches PHP: `explode('|', $hash, 2)` then `(int)$parts[0] > 0`.
fn split_hash(prefixed: &str) -> Option<(u32, &str)> {
    let (head, rest) = prefixed.split_once('|')?;
    let version: u32 = head.parse().ok()?;
    if version > 0 {
        Some((version, rest))
    } else {
        None
    }
}

/// Verify against a raw (unprefixed) PHC/crypt hash, dispatching on the `$…$`
/// algorithm identifier — the Rust equivalent of PHP's `password_verify()`.
fn verify_hash(message: &str, hash: &str) -> bool {
    if hash.starts_with("$argon2") {
        // `PasswordHash::new` parses the PHC string (identifier, version,
        // params, salt, output); `Argon2::verify_password` reads the algorithm
        // back out of the parsed hash, so this one call covers argon2id,
        // argon2i and argon2d — matching `password_verify`'s auto-detection.
        match PasswordHash::new(hash) {
            Ok(parsed) => Argon2::default()
                .verify_password(message.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    } else if hash.starts_with("$2") {
        // bcrypt (`$2y$` from PHP, also `$2a$`/`$2b$`).
        bcrypt::verify(message, hash).unwrap_or(false)
    } else {
        // Unknown PHC identifier — PHP's `password_verify` returns false.
        false
    }
}

/// Legacy (unprefixed) verification — PHP `Hasher::legacyHashVerify`.
///
/// Two attempts, each trying the salted form first then the unsalted form:
/// 1. 60-char hash → PHPass/bcrypt `password_verify($message . $salt, $hash)`
/// 2. 40-char hash → `hash_equals($hash, sha1($message))`
/// then both again with an **empty** salt (for installs that had no
/// `passwordsalt`).
fn legacy_hash_verify(message: &str, hash: &str, legacy_salt: &str) -> bool {
    let salted = format!("{message}{legacy_salt}");
    if legacy_single(&salted, message, hash, legacy_salt) {
        return true;
    }
    // Retry with empty salt.
    legacy_single(message, message, hash, "")
}

/// One pass of the legacy check.  `candidate_bcrypt` is the string fed to the
/// 60-char PHPass/bcrypt path (`$message . $salt`, or `$message` for the
/// empty-salt retry); `message` is the bare message for the 40-char SHA-1 path
/// (SHA-1 never uses the salt).
fn legacy_single(candidate_bcrypt: &str, message: &str, hash: &str, _salt: &str) -> bool {
    match hash.len() {
        60 => {
            // PHPass (`$P$`/`$H$`) or legacy bcrypt (`$2…$`).  We support the
            // bcrypt form; portable-PHPass (MD5-crypt) is a pre-ownCloud-8
            // format that cannot exist on a Nextcloud install and is therefore
            // intentionally not implemented (documented deviation).
            if hash.starts_with("$2") {
                bcrypt::verify(candidate_bcrypt, hash).unwrap_or(false)
            } else {
                tracing::warn!(
                    "legacy PHPass (non-bcrypt) password hash encountered but unsupported — \
                     user must reset their password to migrate to argon2id"
                );
                false
            }
        }
        40 => ct_eq_sha1(message, hash),
        _ => false,
    }
}

/// Constant-time comparison of `sha1(message)` (lowercase hex) against `hash`
/// — PHP `hash_equals($hash, sha1($message))`.
fn ct_eq_sha1(message: &str, hash: &str) -> bool {
    let mut hasher = Sha1::new();
    hasher.update(message.as_bytes());
    let digest = hex::encode(hasher.finalize());
    let expected = hash.to_ascii_lowercase();
    constant_time_eq(digest.as_bytes(), expected.as_bytes())
}

/// Byte-wise constant-time equality (PHP `hash_equals` semantics).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── split_hash ────────────────────────────────────────────────────────
    #[test]
    fn split_hash_versioned() {
        assert_eq!(split_hash("3|$argon2id$x"), Some((3, "$argon2id$x")));
        assert_eq!(split_hash("1|$2y$abc"), Some((1, "$2y$abc")));
    }

    #[test]
    fn split_hash_only_first_pipe() {
        // PHP `explode('|', …, 2)` — version is the first segment only.
        assert_eq!(split_hash("2|a|b"), Some((2, "a|b")));
    }

    #[test]
    fn split_hash_unprefixed_is_none() {
        assert_eq!(split_hash("$argon2id$v=19$xyz"), None);
        assert_eq!(split_hash("da39a3ee5e6b4b0d3255bfef95601890afd80709"), None);
    }

    #[test]
    fn split_hash_zero_version_is_none() {
        // `(int)$parts[0] > 0` — `0|…` is not a valid version.
        assert_eq!(split_hash("0|$2y$x"), None);
    }

    // ── verify_hash: bcrypt dispatch ──────────────────────────────────────
    #[test]
    fn verify_hash_bcrypt() {
        // bcrypt hash of "hunter2" at cost 4 (fast, deterministic salt).
        let hash = bcrypt::hash("hunter2", 4).unwrap();
        assert!(verify_hash("hunter2", &hash));
        assert!(!verify_hash("wrong", &hash));
    }

    // ── verify_hash: argon2id dispatch (the modern default) ───────────────
    #[test]
    fn verify_hash_argon2id() {
        // Generate a real argon2id PHC string via the same crate path PHP's
        // output parses through.  Fixed 16-byte salt keeps the test free of an
        // RNG dependency.
        use argon2::PasswordHasher;
        use argon2::password_hash::SaltString;
        let salt = SaltString::encode_b64(b"0123456789abcdef").unwrap();
        let phc = Argon2::default()
            .hash_password(b"s3cret", &salt)
            .unwrap()
            .to_string();
        assert!(phc.starts_with("$argon2id$"));
        assert!(verify_hash("s3cret", &phc));
        assert!(!verify_hash("nope", &phc));
    }

    // ── end-to-end: versioned prefix routing ──────────────────────────────
    #[test]
    fn verify_password_versioned_bcrypt() {
        let hash = bcrypt::hash("pw", 4).unwrap();
        assert!(verify_password("pw", &format!("1|{hash}"), ""));
        assert!(!verify_password("bad", &format!("1|{hash}"), ""));
    }

    #[test]
    fn verify_password_unknown_version_is_false() {
        let hash = bcrypt::hash("pw", 4).unwrap();
        assert!(!verify_password("pw", &format!("9|{hash}"), ""));
    }

    // ── legacy SHA-1 path ─────────────────────────────────────────────────
    #[test]
    fn verify_password_legacy_sha1() {
        // sha1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        let sha = "a9993e364706816aba3e25717850c26c9cd0d89d";
        assert!(verify_password("abc", sha, ""));
        assert!(verify_password("abc", &sha.to_uppercase(), "")); // case-insensitive
        assert!(!verify_password("abd", sha, ""));
    }

    // ── constant-time eq ──────────────────────────────────────────────────
    #[test]
    fn ct_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
