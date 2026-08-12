//! `X-OC-MTime` / `X-OC-CTime` header sanitization.
//!
//! Mirrors PHP `MtimeSanitizer::sanitizeMtime()`:
//! - Rejects hexadecimal notation (starts with `0x` or `0X`).
//! - Rejects non-numeric values.
//! - Rejects timestamps `<= 86400` (one day in seconds).
//!
//! Used by simple PUT, chunked MOVE assembly, and bulk upload.

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
}
