//! `X-OC-MTime` / `X-OC-CTime` header sanitization.
//!
//! Mirrors PHP `MtimeSanitizer::sanitizeMtime()`:
//! - Rejects hexadecimal notation (starts with `0x` or `0X`).
//! - Rejects non-numeric values.
//! - Rejects timestamps `<= 86400` (one day in seconds).
//!
//! Used by simple PUT, chunked MOVE assembly, and bulk upload.
//!
//! Also houses the media-mtime fallback (improvements.md): the iOS client
//! sends `X-OC-MTime` from `PHAsset.modificationDate ?? Date()` — when the
//! asset has no modification date (WhatsApp-saved images, `NCCameraRoll.swift:237`)
//! the fallback stamps the *upload instant*, which lands the photo on the
//! upload day in the client's Photos tab (sorted by `d:getlastmodified`).
//! Media uploads whose `X-OC-MTime` matches that signature receive
//! `X-OC-CTime` (the true capture/save date, sent by the same client) as
//! their effective mtime instead.

/// Validate and parse an `X-OC-MTime` or `X-OC-CTime` header value.
///
/// Returns:
/// - `Ok(Some(timestamp))` — the sanitized Unix timestamp.
/// - `Ok(None)` — header was absent or empty; caller should skip the touch.
/// - `Err(message)` — header was present but invalid.
///
/// Error messages always reference `X-OC-MTime` (even when called for
/// `X-OC-CTime`), matching PHP's `MtimeSanitizer` which was designed
/// for mtime and reused for ctime without updating the message strings.
pub(crate) fn sanitize_mtime(value: Option<&str>) -> Result<Option<i64>, String> {
    let raw = match value {
        Some(v) if !v.is_empty() => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed
        }
        _ => return Ok(None),
    };

    // ── Hexadecimal check ────────────────────────────────────────────────
    // PHP: preg_match('/^\s*0[xX]/', $mtimeFromRequest)
    if raw.starts_with("0x") || raw.starts_with("0X") {
        return Err(format!(
            "X-OC-MTime header must be a valid integer (unix timestamp), got \"{}\".",
            raw
        ));
    }

    // ── Numeric check ────────────────────────────────────────────────────
    // PHP uses `is_numeric()` which accepts floats, scientific notation, and
    // integers — then casts with `(int)` to truncate.  We parse as f64 first
    // to match that leniency, then discard the fractional part.
    let parsed: f64 = raw.parse().map_err(|_| {
        tracing::warn!(
            header_value = %raw,
            "X-OC-MTime / X-OC-CTime header rejected: not numeric"
        );
        format!(
            "X-OC-MTime header must be a valid integer (unix timestamp), got \"{}\".",
            raw
        )
    })?;

    // ── Bounds check: <= 86400 (24*60*60) is rejected ────────────────────
    // PHP: "must be a valid positive unix timestamp greater than one day"
    if parsed <= 86_400.0 {
        return Err(format!(
            "X-OC-MTime header must be a valid positive unix timestamp greater than one day, got \"{}\".",
            raw
        ));
    }

    // PHP casts with (int) — truncates toward zero (same as `as i64`)
    Ok(Some(parsed as i64))
}

// ─── Media-mtime fallback (intentional divergence, improvements.md) ───────────

/// Window before the server-observed arrival anchor within which an
/// `X-OC-MTime` is treated as the client's `?? Date()` upload-instant
/// fallback.  15 minutes absorbs client↔server clock skew, local chunk
/// splitting, and slow first-chunk delivery.  False positives require media
/// that was genuinely fresh (modified within the window) yet has an older
/// ctime — where ctime is the placement the Photos tab wants anyway.
pub(crate) const MEDIA_MTIME_FALLBACK_WINDOW_SECS: i64 = 15 * 60;

/// Resolve the effective mtime for an upload, applying the media fallback.
///
/// Fires only when ALL of the following hold:
/// - the feature switch is enabled (config `media_mtime_ctime_fallback`);
/// - the target file is media (`image/*` or `video/*` mimetype);
/// - the client sent **both** headers (a headerless request keeps PHP
///   semantics — `mtime` = arrival time is never rewritten);
/// - the sent `X-OC-MTime` falls within `MEDIA_MTIME_FALLBACK_WINDOW_SECS`
///   before `anchor` (the server-observed arrival: request open time for a
///   plain PUT, the earliest chunk's arrival for a chunked MOVE — the chunk
///   PUTs carry no `X-OC-MTime`, so their disk mtimes are arrival times);
/// - `ctime` is strictly older than `mtime` (the mtime is never moved
///   forward).
///
/// When the fallback fires, `X-OC-CTime` is returned as the effective mtime;
/// otherwise `mtime` is returned unchanged.
pub(crate) fn media_mtime_fallback(
    mtime: i64,
    mtime_header: Option<i64>,
    ctime_header: Option<i64>,
    anchor: i64,
    is_media: bool,
    enabled: bool,
) -> i64 {
    if !enabled || !is_media {
        return mtime;
    }
    let (Some(sent_mtime), Some(ctime)) = (mtime_header, ctime_header) else {
        return mtime;
    };
    if ctime >= sent_mtime {
        return mtime;
    }
    if sent_mtime < anchor - MEDIA_MTIME_FALLBACK_WINDOW_SECS {
        return mtime;
    }
    tracing::debug!(
        mtime = sent_mtime,
        ctime = ctime,
        anchor = anchor,
        "media upload: X-OC-MTime matches the client now-fallback — using X-OC-CTime as mtime"
    );
    ctime
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Happy path ───────────────────────────────────────────────────────

    #[test]
    fn none_when_absent() {
        assert_eq!(sanitize_mtime(None).unwrap(), None);
    }

    #[test]
    fn none_when_empty() {
        assert_eq!(sanitize_mtime(Some("")).unwrap(), None);
        assert_eq!(sanitize_mtime(Some("  ")).unwrap(), None);
    }

    #[test]
    fn valid_timestamp() {
        assert_eq!(
            sanitize_mtime(Some("1234567890")).unwrap(),
            Some(1234567890)
        );
    }

    #[test]
    fn trimmed_timestamp() {
        assert_eq!(
            sanitize_mtime(Some("  1234567890  ")).unwrap(),
            Some(1234567890)
        );
    }

    // ── Hexadecimal rejection ────────────────────────────────────────────

    #[test]
    fn hex_prefix_rejected() {
        let err = sanitize_mtime(Some("0x123")).unwrap_err();
        assert!(err.contains("valid integer"));
    }

    #[test]
    fn hex_prefix_uppercase_rejected() {
        let err = sanitize_mtime(Some("0XABC")).unwrap_err();
        assert!(err.contains("valid integer"));
    }

    #[test]
    fn hex_prefix_with_whitespace_rejected() {
        let err = sanitize_mtime(Some("  0x42")).unwrap_err();
        assert!(err.contains("valid integer"));
    }

    // ── Non-numeric rejection ────────────────────────────────────────────

    #[test]
    fn non_numeric_rejected() {
        let err = sanitize_mtime(Some("abc")).unwrap_err();
        assert!(err.contains("valid integer"));
    }

    #[test]
    fn decimal_accepted_truncated() {
        // PHP's is_numeric() accepts floats; (int) cast truncates.
        // "123.45" → truncates to 123 (rejected by bounds anyway, but parse succeeds)
        assert_eq!(
            sanitize_mtime(Some("1234567890.8558369")).unwrap(),
            Some(1234567890)
        );
    }

    // ── Bounds rejection ─────────────────────────────────────────────────

    #[test]
    fn zero_rejected() {
        let err = sanitize_mtime(Some("0")).unwrap_err();
        assert!(err.contains("greater than one day"));
    }

    #[test]
    fn exactly_one_day_rejected() {
        // 86400 = 24*60*60 — the boundary itself is rejected
        let err = sanitize_mtime(Some("86400")).unwrap_err();
        assert!(err.contains("greater than one day"));
    }

    #[test]
    fn just_below_one_day_rejected() {
        let err = sanitize_mtime(Some("86399")).unwrap_err();
        assert!(err.contains("greater than one day"));
    }

    #[test]
    fn just_above_one_day_accepted() {
        assert_eq!(sanitize_mtime(Some("86401")).unwrap(), Some(86401));
    }

    // ── Negative values ──────────────────────────────────────────────────

    #[test]
    fn negative_rejected() {
        let err = sanitize_mtime(Some("-1234567890")).unwrap_err();
        assert!(err.contains("greater than one day"));
    }

    // ── Media-mtime fallback ─────────────────────────────────────────────
    //
    // All scenarios model the WhatsApp-photo case: X-OC-CTime = save date
    // (e.g. 2024-01-01), X-OC-MTime = the client's `?? Date()` upload-instant
    // fallback (≈ anchor), anchor = server-observed arrival (PUT request
    // open time, or the earliest chunk's disk mtime).

    const SAVE_CTIME: i64 = 1_704_067_200; // 2024-01-01T00:00:00Z
    const ANCHOR: i64 = 1_784_120_000;     // upload instant (server clock)

    #[test]
    fn fires_on_client_now_fallback() {
        assert_eq!(
            media_mtime_fallback(ANCHOR, Some(ANCHOR), Some(SAVE_CTIME), ANCHOR, true, true),
            SAVE_CTIME
        );
    }

    #[test]
    fn fires_within_window_before_anchor() {
        // WhatsApp photo chunked: extraction → upload took 10 minutes.
        let sent = ANCHOR - 10 * 60;
        assert_eq!(
            media_mtime_fallback(sent, Some(sent), Some(SAVE_CTIME), ANCHOR, true, true),
            SAVE_CTIME
        );
    }

    #[test]
    fn boundary_exactly_at_window_fires() {
        let sent = ANCHOR - MEDIA_MTIME_FALLBACK_WINDOW_SECS;
        assert_eq!(
            media_mtime_fallback(sent, Some(sent), Some(SAVE_CTIME), ANCHOR, true, true),
            SAVE_CTIME
        );
    }

    #[test]
    fn genuine_old_mtime_untouched() {
        // A real camera photo from 2023: mtime far outside the window.
        let sent = 1_676_160_000;
        assert_eq!(
            media_mtime_fallback(sent, Some(sent), Some(SAVE_CTIME), ANCHOR, true, true),
            sent
        );
    }

    #[test]
    fn edited_photo_modified_inside_window_reanchors() {
        // Accepted trade-off of the 15-minute window: media modified within
        // the window before arrival is indistinguishable from the client
        // fallback, so it re-anchors to ctime — for a photo shot an hour
        // before the edit, the Photos tab then shows the capture day.
        let sent = ANCHOR - 5 * 60;
        let edited_ctime = sent - 3600; // shot an hour before the edit
        assert_eq!(
            media_mtime_fallback(sent, Some(sent), Some(edited_ctime), ANCHOR, true, true),
            edited_ctime
        );
    }

    #[test]
    fn edited_photo_modified_outside_window_untouched() {
        // Edited 20 minutes before upload: outside the window → the real
        // modification date is kept.
        let sent = ANCHOR - 20 * 60;
        let edited_ctime = sent - 3600;
        assert_eq!(
            media_mtime_fallback(sent, Some(sent), Some(edited_ctime), ANCHOR, true, true),
            sent
        );
    }

    #[test]
    fn switch_disabled_keeps_php_semantics() {
        assert_eq!(
            media_mtime_fallback(ANCHOR, Some(ANCHOR), Some(SAVE_CTIME), ANCHOR, true, false),
            ANCHOR
        );
    }

    #[test]
    fn non_media_untouched() {
        assert_eq!(
            media_mtime_fallback(ANCHOR, Some(ANCHOR), Some(SAVE_CTIME), ANCHOR, false, true),
            ANCHOR
        );
    }

    #[test]
    fn missing_mtime_header_untouched() {
        // Headerless X-OC-MTime: PHP semantics — mtime = arrival, never
        // rewritten by the fallback.
        assert_eq!(
            media_mtime_fallback(ANCHOR, None, Some(SAVE_CTIME), ANCHOR, true, true),
            ANCHOR
        );
    }

    #[test]
    fn missing_ctime_header_untouched() {
        assert_eq!(
            media_mtime_fallback(ANCHOR, Some(ANCHOR), None, ANCHOR, true, true),
            ANCHOR
        );
    }

    #[test]
    fn ctime_not_older_than_mtime_untouched() {
        // ctime after mtime would move the mtime forward — never applied.
        let ctime = ANCHOR + 60;
        assert_eq!(
            media_mtime_fallback(ANCHOR, Some(ANCHOR), Some(ctime), ANCHOR, true, true),
            ANCHOR
        );
    }

    #[test]
    fn no_chunk_metadata_untouched() {
        // first_chunk_mtime could not be stat'd — anchor stays i64::MAX, so
        // the window check can never fire.
        assert_eq!(
            media_mtime_fallback(ANCHOR, Some(ANCHOR), Some(SAVE_CTIME), i64::MAX, true, true),
            ANCHOR
        );
    }

    #[test]
    fn fractional_values_already_truncated_by_sanitizer() {
        // The sanitizer truncates before this resolver runs (X-OC-MTime
        // "1718841600.123456" → 1718841600).
        let sent = ANCHOR - 1;
        assert_eq!(
            media_mtime_fallback(sent, Some(sent), Some(SAVE_CTIME), ANCHOR, true, true),
            SAVE_CTIME
        );
    }
}
