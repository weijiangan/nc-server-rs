//! Timestamps in Nextcloud's storage convention.
//!
//! Every time column Nextcloud writes (`oc_filecache.mtime`,
//! `oc_authtoken.last_activity`, `oc_files_trash.timestamp`, …) is Unix
//! seconds as a signed 64-bit integer, so the conversion from `SystemTime`
//! is spelled the same way at every call site.

/// The current time as Unix seconds.
///
/// Saturates to `0` for pre-epoch clocks rather than panicking — a bogus
/// system clock must not take the server down.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
