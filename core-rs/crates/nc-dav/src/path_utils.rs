use dav_server::fs::DavProp;

/// Extract the file extension from a filename.
pub(crate) fn extension(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or("")
}

/// Check the PHP trashbin extension convention: `d` followed by digits.
pub(crate) fn is_trash_extension(ext: &str) -> bool {
    if ext.len() < 2 || !ext.starts_with('d') {
        return false;
    }
    ext[1..].chars().all(|c| c.is_ascii_digit())
}

/// Percent-encode a URI path while preserving path separators and RFC 3986
/// path-allowed characters.
pub(crate) fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/'
            | b':'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'=' => out.push(byte as char),
            _ => {
                out.push('%');
                let hi = byte >> 4;
                let lo = byte & 0xF;
                out.push(
                    char::from_digit(hi as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
                out.push(
                    char::from_digit(lo as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
            }
        }
    }
    out
}

/// Extract the text content from a DAV property's raw XML bytes.
pub(crate) fn extract_text_from_prop_xml(prop: &DavProp) -> Option<String> {
    let xml = prop.xml.as_ref()?;
    let s = std::str::from_utf8(xml).ok()?;
    let start = s.find('>')? + 1;
    let end = s.rfind('<')?;
    if start < end {
        Some(s[start..end].to_string())
    } else {
        None
    }
}

/// Parse the RFC 3339 subset used by WebDAV creation dates as UTC.
pub(crate) fn parse_iso8601(s: &str) -> Option<i64> {
    let core = if s.ends_with('Z') {
        &s[..s.len() - 1]
    } else if let Some(pos) = s.rfind('+').filter(|&p| p >= 10) {
        &s[..pos]
    } else if let Some(pos) = s[10..].rfind('-').map(|p| p + 10) {
        &s[..pos]
    } else {
        s
    };

    if core.len() < 19 {
        return None;
    }

    let year: i64 = core[0..4].parse().ok()?;
    let month: i64 = core[5..7].parse().ok()?;
    let day: i64 = core[8..10].parse().ok()?;
    let hour: i64 = core[11..13].parse().ok()?;
    let min: i64 = core[14..16].parse().ok()?;
    let sec: i64 = core[17..19].parse().ok()?;

    let a = (14 - month) / 12;
    let y = year + 4800 - a;
    let m = month + 12 * a - 3;
    let jdn = day + (153 * m + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32045;
    let unix_days = jdn - 2_440_588;

    Some(unix_days * 86_400 + hour * 3_600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::{extension, is_trash_extension, parse_iso8601};

    #[test]
    fn iso8601_z_suffix() {
        // python3: datetime(2024,1,15,12,34,56,tz=UTC).timestamp() == 1705322096
        assert_eq!(parse_iso8601("2024-01-15T12:34:56Z"), Some(1705322096));
    }

    #[test]
    fn iso8601_plus_offset_ignored() {
        // Offset is stripped so both forms give the same raw numbers treated as UTC
        assert_eq!(parse_iso8601("2024-01-15T12:34:56+02:00"), Some(1705322096));
    }

    #[test]
    fn iso8601_unix_epoch() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
    }

    // ── §10.10 helper tests ──────────────────────────────────────────────

    #[test]
    fn extension_simple() {
        assert_eq!(extension("photo.jpg"), "jpg");
        assert_eq!(extension("note.txt"), "txt");
        assert_eq!(extension("archive.tar.gz"), "gz");
    }

    #[test]
    fn extension_no_dot() {
        assert_eq!(extension("Makefile"), "Makefile");
        assert_eq!(extension("noextension"), "noextension");
    }

    #[test]
    fn extension_hidden_file() {
        assert_eq!(extension(".bashrc"), "bashrc");
    }

    #[test]
    fn is_trash_extension_valid() {
        assert!(is_trash_extension("d1634567890"));
        assert!(is_trash_extension("d0"));
    }

    #[test]
    fn is_trash_extension_invalid() {
        assert!(!is_trash_extension("txt"));
        assert!(!is_trash_extension("d")); // too short
        assert!(!is_trash_extension("dx123")); // has non-digit
        assert!(!is_trash_extension("")); // empty
    }
}
