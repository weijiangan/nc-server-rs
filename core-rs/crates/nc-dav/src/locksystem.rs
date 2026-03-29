//! `NcLockSystem` — Nextcloud fake lock system for DAV class 2 compliance.
//!
//! Implements `DavLockSystem` as a stateless no-op locker, matching
//! SabreDAV's `FakeLockerPlugin` semantics required by macOS Finder,
//! OneNote, and Microsoft WebDAVFS (REQ §6.3 / IMPL_PLAN §4 DavLockSystem).
//!
//! Key differences from `dav_server::fakels::FakeLs`:
//! - Lock token is **deterministic**: `urn:uuid:{md5_hex(path)}` instead of
//!   a random UUID, matching the PHP `FakeLockerPlugin` token derivation.
//! - Timeout is **1800 seconds** (30 min) rather than 120 s, matching the
//!   PHP reference implementation.
//! - `check()` always returns `Ok(())` so every `If:` header lock token is
//!   treated as valid — no actual lock state is maintained.
//! - `discover()` always returns an empty list so `{DAV:}lockdiscovery`
//!   PROPFIND responses show no active locks.

use std::time::{Duration, SystemTime};

use dav_server::davpath::DavPath;
use dav_server::ls::{DavLock, DavLockSystem, LsFuture};
use futures::FutureExt as _;
use md5::{Digest as _, Md5};
use xmltree::Element;

/// Timeout used for all issued fake lock tokens (matches PHP FakeLockerPlugin).
const LOCK_TIMEOUT_SECS: u64 = 1800;

/// Nextcloud fake lock system.
///
/// All state is stateless: every method either derives its answer from the
/// inputs (e.g. token from path) or returns a trivial success.
#[derive(Debug, Clone)]
pub struct NcLockSystem;

impl NcLockSystem {
    /// Create a new `NcLockSystem`, boxed to satisfy `DavConfig::locksystem`.
    pub fn new() -> Box<Self> {
        Box::new(NcLockSystem)
    }

    /// Derive a deterministic lock token from a path.
    ///
    /// Returns `urn:uuid:{md5_hex(path)}`, e.g.
    /// `urn:uuid:d41d8cd98f00b204e9800998ecf8427e` for the empty path.
    fn token_for(path: &DavPath) -> String {
        let path_str = path.as_url_string();
        let hash = Md5::digest(path_str.as_bytes());
        format!("urn:uuid:{:x}", hash)
    }
}

impl Default for NcLockSystem {
    fn default() -> Self {
        NcLockSystem
    }
}

impl DavLockSystem for NcLockSystem {
    /// Returns a fake lock with a deterministic token (`urn:uuid:{md5(path)}`).
    ///
    /// Always succeeds (returns `Ok`). The token is stable across calls for
    /// the same path, which means concurrent LOCK requests for the same
    /// resource produce the same token — consistent with stateless fake locking.
    fn lock(
        &'_ self,
        path: &DavPath,
        principal: Option<&str>,
        owner: Option<&Element>,
        _timeout: Option<Duration>,
        shared: bool,
        deep: bool,
    ) -> LsFuture<'_, Result<DavLock, DavLock>> {
        let timeout = Duration::from_secs(LOCK_TIMEOUT_SECS);
        let timeout_at = SystemTime::now() + timeout;
        let token = Self::token_for(path);

        let lock = DavLock {
            token,
            path: Box::new(path.clone()),
            principal: principal.map(|s| s.to_string()),
            owner: owner.map(|o| Box::new(o.clone())),
            timeout_at: Some(timeout_at),
            timeout: Some(timeout),
            shared,
            deep,
        };
        std::future::ready(Ok(lock)).boxed()
    }

    /// Always succeeds — no state is maintained.
    fn unlock(&'_ self, _path: &DavPath, _token: &str) -> LsFuture<'_, Result<(), ()>> {
        std::future::ready(Ok(())).boxed()
    }

    /// Refresh: re-derive the token for the path and return an updated lock.
    fn refresh(
        &'_ self,
        path: &DavPath,
        token: &str,
        _timeout: Option<Duration>,
    ) -> LsFuture<'_, Result<DavLock, ()>> {
        let timeout = Duration::from_secs(LOCK_TIMEOUT_SECS);
        let timeout_at = SystemTime::now() + timeout;

        let lock = DavLock {
            token: token.to_string(),
            path: Box::new(path.clone()),
            principal: None,
            owner: None,
            timeout_at: Some(timeout_at),
            timeout: Some(timeout),
            shared: false,
            deep: false,
        };
        std::future::ready(Ok(lock)).boxed()
    }

    /// Always valid — every `If:` header token is accepted (REQ §6.3).
    fn check(
        &'_ self,
        _path: &DavPath,
        _principal: Option<&str>,
        _ignore_principal: bool,
        _deep: bool,
        _submitted_tokens: &[String],
    ) -> LsFuture<'_, Result<(), DavLock>> {
        std::future::ready(Ok(())).boxed()
    }

    /// Returns an empty list — `{DAV:}lockdiscovery` shows no active locks.
    fn discover(&'_ self, _path: &DavPath) -> LsFuture<'_, Vec<DavLock>> {
        std::future::ready(Vec::new()).boxed()
    }

    /// No-op — no state to clean up.
    fn delete(&'_ self, _path: &DavPath) -> LsFuture<'_, Result<(), ()>> {
        std::future::ready(Ok(())).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dav_server::davpath::DavPath;

    fn make_path(s: &str) -> DavPath {
        DavPath::new(s).unwrap()
    }

    #[test]
    fn token_is_deterministic() {
        let p = make_path("/dav/files/alice/photo.jpg");
        let t1 = NcLockSystem::token_for(&p);
        let t2 = NcLockSystem::token_for(&p);
        assert_eq!(t1, t2);
        assert!(t1.starts_with("urn:uuid:"), "token should start with urn:uuid:");
    }

    #[test]
    fn token_differs_per_path() {
        let p1 = make_path("/dav/files/alice/a.txt");
        let p2 = make_path("/dav/files/alice/b.txt");
        assert_ne!(
            NcLockSystem::token_for(&p1),
            NcLockSystem::token_for(&p2),
            "distinct paths must produce distinct tokens"
        );
    }

    #[test]
    fn token_format_is_urn_uuid_md5() {
        // md5("") = d41d8cd98f00b204e9800998ecf8427e
        let p = make_path("/");
        let t = NcLockSystem::token_for(&p);
        // Token is urn:uuid: followed by 32 hex chars (md5 of the url string form)
        let hex_part = t.strip_prefix("urn:uuid:").expect("urn:uuid: prefix missing");
        assert_eq!(hex_part.len(), 32, "md5 hex should be 32 chars");
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit()),
            "all chars should be hex"
        );
    }

    #[tokio::test]
    async fn lock_returns_ok_with_1800s_timeout() {
        let ls = NcLockSystem;
        let p = make_path("/dav/files/alice/doc.txt");
        let result = ls.lock(&p, Some("alice"), None, None, false, false).await;
        assert!(result.is_ok());
        let lock = result.unwrap();
        assert_eq!(lock.timeout, Some(Duration::from_secs(1800)));
        assert!(lock.token.starts_with("urn:uuid:"));
    }

    #[tokio::test]
    async fn unlock_always_succeeds() {
        let ls = NcLockSystem;
        let p = make_path("/dav/files/alice/doc.txt");
        let result = ls.unlock(&p, "urn:uuid:deadbeef").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_always_valid() {
        let ls = NcLockSystem;
        let p = make_path("/dav/files/alice/doc.txt");
        let tokens = vec!["urn:uuid:some-random-token".to_string()];
        let result = ls.check(&p, Some("alice"), false, false, &tokens).await;
        assert!(
            result.is_ok(),
            "check() must always accept any submitted token"
        );
    }

    #[tokio::test]
    async fn discover_returns_empty() {
        let ls = NcLockSystem;
        let p = make_path("/dav/files/alice/doc.txt");
        let locks = ls.discover(&p).await;
        assert!(locks.is_empty(), "discover() must return no active locks");
    }
}
