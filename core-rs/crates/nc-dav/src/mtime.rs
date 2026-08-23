//! `X-OC-MTime` / `X-OC-CTime` header sanitization.
//!
//! Mirrors PHP `MtimeSanitizer::sanitizeMtime()`:
//! - Rejects hexadecimal notation (starts with `0x` or `0X`).
//! - Rejects non-numeric values.
//! - Rejects timestamps `<= 86400` (one day in seconds).
//!
//! Used by simple PUT, chunked MOVE assembly, and bulk upload.
//!
//! Also houses the media-mtime override (improvements.md): the iOS client
//! sends `X-OC-MTime` from `PHAsset.modificationDate ?? Date()` — when the
//! asset has no modification date (WhatsApp-saved images, `NCCameraRoll.swift:237`)
//! the fallback stamps the *upload instant*, which lands the photo on the
//! upload day in the client's Photos tab (sorted by `d:getlastmodified`).
//! `X-OC-CTime` (the true capture/save date, sent by the same client) is the
//! placement that tab wants, so for media uploads it becomes the effective
//! mtime unconditionally (see `media_mtime_ctime_override`).

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

// ─── Media-mtime override (intentional divergence, improvements.md) ───────────

/// Resolve the effective mtime for an upload, applying the flat media override.
///
/// When ALL of the following hold, `X-OC-CTime` becomes the effective mtime
/// with no further conditions — no arrival-window check, no "both headers"
/// requirement, no mtime/ctime ordering constraint:
/// - the feature switch is enabled (config `media_mtime_ctime_fallback`);
/// - the target file is media (`image/*` or `video/*` mimetype);
/// - the client sent a valid `X-OC-CTime`.
///
/// The original windowed heuristic (commit 0563441) anchored on the
/// server-observed arrival (request open time, or the earliest chunk's disk
/// mtime for a chunked MOVE) and refused to fire when the sent `X-OC-MTime`
/// predated that anchor by more than 15 minutes.  That still missed real
/// uploads: the iOS client stamps `?? Date()` at *extraction*, and a deferred
/// background session can land the first chunk well outside the window.  The
/// flat rule removes every heuristic — media + ctime always wins — so the
/// Photos tab (sorted by `d:getlastmodified`) gets the capture/save date.
/// The trade-off: genuinely fresh, *edited* media is also re-anchored to its
/// capture day, and a (pathological) future ctime would move the mtime
/// forward.  Both are accepted for the media case.
///
/// Otherwise (switch off, non-media, or no `X-OC-CTime`) `mtime` is returned
/// unchanged — strict PHP semantics.
pub(crate) fn media_mtime_ctime_override(
    mtime: i64,
    ctime_header: Option<i64>,
    is_media: bool,
    enabled: bool,
) -> i64 {
    if !enabled || !is_media {
        return mtime;
    }
    match ctime_header {
        Some(ctime) => {
            tracing::debug!(
                ctime = ctime,
                "media upload: flat override — using X-OC-CTime as mtime"
            );
            ctime
        }
        None => mtime,
    }
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

    // ── Media-mtime override ─────────────────────────────────────────────
    //
    // All scenarios model the WhatsApp-photo case: X-OC-CTime = save date
    // (e.g. 2024-01-01), the sent/derived mtime = the client's `?? Date()`
    // upload-instant fallback.  The flat rule ignores every other condition:
    // media + ctime always wins.

    const SAVE_CTIME: i64 = 1_704_067_200; // 2024-01-01T00:00:00Z
    const ARRIVAL: i64 = 1_784_120_000;    // upload instant (server clock)

    #[test]
    fn overrides_now_fallback_mtime() {
        assert_eq!(
            media_mtime_ctime_override(ARRIVAL, Some(SAVE_CTIME), true, true),
            SAVE_CTIME
        );
    }

    #[test]
    fn overrides_deferred_background_upload() {
        // The windowed heuristic's blind spot: the iOS client stamps
        // `?? Date()` at extraction, then a background session uploads hours
        // later.  The flat rule overrides regardless of how long passed.
        let sent = ARRIVAL - 6 * 3600;
        assert_eq!(
            media_mtime_ctime_override(sent, Some(SAVE_CTIME), true, true),
            SAVE_CTIME
        );
    }

    #[test]
    fn overrides_genuine_old_mtime() {
        // A real camera photo synced days later: ctime wins over the mtime,
        // matching what the Photos tab wants.
        let sent = 1_676_160_000; // 2023
        assert_eq!(
            media_mtime_ctime_override(sent, Some(SAVE_CTIME), true, true),
            SAVE_CTIME
        );
    }

    #[test]
    fn overrides_future_ctime() {
        // The mtime can now move forward too — accepted for the media case
        // (PHAsset.creationDate is never future in practice).
        let ctime = ARRIVAL + 3600;
        assert_eq!(
            media_mtime_ctime_override(ARRIVAL, Some(ctime), true, true),
            ctime
        );
    }

    #[test]
    fn switch_disabled_keeps_php_semantics() {
        assert_eq!(
            media_mtime_ctime_override(ARRIVAL, Some(SAVE_CTIME), true, false),
            ARRIVAL
        );
    }

    #[test]
    fn non_media_untouched() {
        assert_eq!(
            media_mtime_ctime_override(ARRIVAL, Some(SAVE_CTIME), false, true),
            ARRIVAL
        );
    }

    #[test]
    fn missing_ctime_header_untouched() {
        assert_eq!(
            media_mtime_ctime_override(ARRIVAL, None, true, true),
            ARRIVAL
        );
    }

    #[test]
    fn fractional_values_already_truncated_by_sanitizer() {
        // The sanitizer truncates before this resolver runs (X-OC-CTime
        // "1718841600.123456" → 1718841600).
        assert_eq!(
            media_mtime_ctime_override(ARRIVAL, Some(SAVE_CTIME), true, true),
            SAVE_CTIME
        );
    }
}
