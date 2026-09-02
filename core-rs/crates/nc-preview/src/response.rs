//! Preview response metadata — header / ETag / 304 / Cache-Control parity with PHP.
//!
//! Parity sources (all read from the reference):
//! - `FileDisplayResponse` — `Content-Disposition: inline; filename="<name>"`,
//!   `ETag`/`Last-Modified`/`Content-Length` from the preview row.
//! - `Response::getHeaders` — every response carries the framework defaults
//!   `Cache-Control: no-cache, no-store, must-revalidate` and
//!   `X-Robots-Tag: noindex, nofollow`; a quoted `ETag`.
//! - `Response::cacheFor(86400, false, true)` — the core routes override
//!   `Cache-Control` to `private, max-age=86400, immutable` and add `Expires` (+24 h).
//! - `NotModifiedMiddleware` — exact-string `304` on `If-None-Match` (quoted,
//!   trimmed) first, then `If-Modified-Since` (RFC 7231, trimmed).
//!
//! The ETag is the **source file's etag at generation** (`oc_previews.etag`) and
//! `Last-Modified` is the **generation timestamp** (`oc_previews.mtime`) — both come
//! from the row via [`crate::store::PreviewRow`], never the source file.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `X-Robots-Tag` set on every preview response (`Response::getHeaders` default).
pub const X_ROBOTS_TAG: &str = "noindex, nofollow";

const CACHE_CORE: &str = "private, max-age=86400, immutable";
const CACHE_DEFAULT: &str = "no-cache, no-store, must-revalidate";
const CORE_MAX_AGE_SECS: u64 = 86400;

/// Which route family is serving — drives the `Cache-Control`/`Expires` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    /// `/core/preview` and `/core/preview.png` — `cacheFor(86400, private, immutable)`.
    Core,
    /// `/apps/photos/api/v1/preview/{fileId}` — same `cacheFor(86400, private, immutable)`
    /// policy as Core (`Photos\PreviewController::index` calls `cacheFor(3600*24, false, true)`).
    Photos,
    /// `/apps/files/api/v1/thumbnail/{x}/{y}/{file}` — framework default (no `cacheFor`).
    FilesThumbnail,
}

/// A fully-decided preview response.  When `status == 200` the caller streams
/// `content_length` bytes from the row's byte path; for `304` no body is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewResponse {
    pub status: u16,
    /// Output mimetype (`oc_previews.mimetype`, resolved from the row's id).
    pub content_type: String,
    /// Quoted strong ETag, e.g. `"\"abc123\""`.
    pub etag: String,
    /// RFC 7231 `Last-Modified` value (from `oc_previews.mtime`).
    pub last_modified: String,
    /// `inline; filename="<preview name>"`.
    pub content_disposition: String,
    pub cache_control: String,
    /// RFC 7231 `Expires` (`Core` only; `None` for the files thumbnail route).
    pub expires: Option<String>,
    /// Body length for a `200` (`0` for a `304`).
    pub content_length: i64,
}

/// Format a UNIX timestamp as an RFC 7231 IMF-fixdate (PHP `Constants::DATE_RFC7231`,
/// e.g. `Thu, 01 Jan 1970 00:00:00 GMT`).  Negative timestamps clamp to the epoch.
pub fn rfc7231(unix_secs: i64) -> String {
    let secs = unix_secs.max(0) as u64;
    httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(secs))
}

/// `NotModifiedMiddleware` parity: `If-None-Match` (quoted, trimmed, exact) is tested
/// first; if it is present but does not match, falls through to `If-Modified-Since`
/// (trimmed, exact RFC 7231).  Returns `true` when the client's copy is current (304).
pub fn is_not_modified(
    if_none_match: Option<&str>,
    if_modified_since: Option<&str>,
    etag_unquoted: &str,
    last_modified_unix: i64,
) -> bool {
    if let Some(inm) = if_none_match.map(str::trim).filter(|s| !s.is_empty()) {
        if inm == format!("\"{etag_unquoted}\"") {
            return true;
        }
        // Present but not matching → fall through to If-Modified-Since (PHP does too).
    }
    if let Some(ims) = if_modified_since.map(str::trim).filter(|s| !s.is_empty()) {
        if ims == rfc7231(last_modified_unix) {
            return true;
        }
    }
    false
}

/// Build the response metadata for a matched preview row.
///
/// * `etag_unquoted` — `oc_previews.etag` (source file's etag at generation).
/// * `mtime_unix` — `oc_previews.mtime` (generation timestamp).
/// * `file_name` — the preview's on-disk name (`[version-]w-h[-crop][-max].ext`).
///   PHP applies `rawurldecode`, an identity for generated names (digits, `-`, `.`,
///   lowercase extension), so it is used verbatim.
/// * `now` — current time, for the `Expires` header (injected for testability).
#[allow(clippy::too_many_arguments)]
pub fn build_preview_response(
    kind: RouteKind,
    output_mime: &str,
    etag_unquoted: &str,
    mtime_unix: i64,
    file_name: &str,
    size: i64,
    if_none_match: Option<&str>,
    if_modified_since: Option<&str>,
    now: SystemTime,
) -> PreviewResponse {
    let not_modified = is_not_modified(if_none_match, if_modified_since, etag_unquoted, mtime_unix);
    let (cache_control, expires) = match kind {
        RouteKind::Core | RouteKind::Photos => (
            CACHE_CORE.to_string(),
            Some(httpdate::fmt_http_date(
                now + Duration::from_secs(CORE_MAX_AGE_SECS),
            )),
        ),
        RouteKind::FilesThumbnail => (CACHE_DEFAULT.to_string(), None),
    };
    PreviewResponse {
        status: if not_modified { 304 } else { 200 },
        content_type: output_mime.to_string(),
        etag: format!("\"{etag_unquoted}\""),
        last_modified: rfc7231(mtime_unix),
        content_disposition: format!("inline; filename=\"{file_name}\""),
        cache_control,
        expires,
        content_length: if not_modified { 0 } else { size },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    // ── RFC 7231 date formatting ───────────────────────────────────────────

    #[test]
    fn rfc7231_formats_imf_fixdate() {
        assert_eq!(rfc7231(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // 2023-11-14T22:13:20Z
        assert_eq!(rfc7231(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
        // negative clamps to epoch
        assert_eq!(rfc7231(-5), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    // ── 304 / NotModifiedMiddleware ────────────────────────────────────────

    #[test]
    fn not_modified_on_matching_etag() {
        assert!(is_not_modified(Some("\"abc\""), None, "abc", 1_700_000_000));
        // surrounding whitespace trimmed
        assert!(is_not_modified(
            Some("  \"abc\"  "),
            None,
            "abc",
            1_700_000_000
        ));
        // wrong etag → not 304 on this check
        assert!(!is_not_modified(
            Some("\"other\""),
            None,
            "abc",
            1_700_000_000
        ));
        // unquoted client value does not match the quoted server etag
        assert!(!is_not_modified(Some("abc"), None, "abc", 1_700_000_000));
    }

    #[test]
    fn not_modified_on_matching_last_modified() {
        let lm = rfc7231(1_700_000_000);
        assert!(is_not_modified(None, Some(&lm), "abc", 1_700_000_000));
        assert!(is_not_modified(
            None,
            Some(&format!("  {lm}  ")),
            "abc",
            1_700_000_000
        ));
        assert!(!is_not_modified(
            None,
            Some("Tue, 14 Nov 2023 00:00:00 GMT"),
            "abc",
            1_700_000_000
        ));
    }

    #[test]
    fn etag_mismatch_falls_through_to_last_modified() {
        // If-None-Match present but wrong → PHP still checks If-Modified-Since.
        let lm = rfc7231(1_700_000_000);
        assert!(is_not_modified(
            Some("\"wrong\""),
            Some(&lm),
            "abc",
            1_700_000_000
        ));
        // both wrong → not modified is false
        assert!(!is_not_modified(
            Some("\"wrong\""),
            Some("nope"),
            "abc",
            1_700_000_000
        ));
    }

    #[test]
    fn empty_headers_are_ignored() {
        assert!(!is_not_modified(Some(""), Some(""), "abc", 1_700_000_000));
        assert!(!is_not_modified(None, None, "abc", 1_700_000_000));
    }

    // ── build_preview_response: headers per route ──────────────────────────

    #[test]
    fn core_route_caches_24h_immutable() {
        let r = build_preview_response(
            RouteKind::Core,
            "image/jpeg",
            "srcetag",
            1_700_000_000,
            "256-256-crop.jpg",
            4096,
            None,
            None,
            fixed_now(),
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "image/jpeg");
        assert_eq!(r.etag, "\"srcetag\"");
        assert_eq!(r.last_modified, "Tue, 14 Nov 2023 22:13:20 GMT");
        assert_eq!(
            r.content_disposition,
            "inline; filename=\"256-256-crop.jpg\""
        );
        assert_eq!(r.cache_control, "private, max-age=86400, immutable");
        // Expires = now + 86400
        assert_eq!(
            r.expires.as_deref(),
            Some(rfc7231(1_700_000_000 + 86400)).as_deref()
        );
        assert_eq!(r.content_length, 4096);
    }

    #[test]
    fn photos_route_caches_24h_immutable() {
        let r = build_preview_response(
            RouteKind::Photos,
            "image/jpeg",
            "srcetag",
            1_700_000_000,
            "256-256.jpg",
            4096,
            None,
            None,
            fixed_now(),
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "image/jpeg");
        assert_eq!(r.cache_control, "private, max-age=86400, immutable");
        assert_eq!(
            r.expires.as_deref(),
            Some(rfc7231(1_700_000_000 + 86400)).as_deref()
        );
    }

    #[test]
    fn files_route_uses_default_no_store_and_no_expires() {
        let r = build_preview_response(
            RouteKind::FilesThumbnail,
            "image/png",
            "e",
            1_700_000_000,
            "64-64.png",
            100,
            None,
            None,
            fixed_now(),
        );
        assert_eq!(r.cache_control, "no-cache, no-store, must-revalidate");
        assert_eq!(r.expires, None);
    }

    #[test]
    fn not_modified_returns_304_with_zero_body() {
        let r = build_preview_response(
            RouteKind::Core,
            "image/jpeg",
            "srcetag",
            1_700_000_000,
            "256-256-crop.jpg",
            4096,
            Some("\"srcetag\""),
            None,
            fixed_now(),
        );
        assert_eq!(r.status, 304);
        assert_eq!(r.content_length, 0);
        // headers still present on a 304
        assert_eq!(r.etag, "\"srcetag\"");
        assert_eq!(r.last_modified, "Tue, 14 Nov 2023 22:13:20 GMT");
    }
}
