//! Filename validation — mirrors PHP `FilenameValidator::validateFilename()`.
//!
//! Rules applied (in order):
//! 1. Empty or whitespace-only → rejected
//! 2. `.` or `..` (reserved directory names) → rejected
//! 3. Longer than 250 characters → rejected  (`oc_filecache.name` is VARCHAR(250))
//! 4. Contains a byte in the range 0–31 (control characters) → rejected
//! 5. Contains `\` or `/` (always-forbidden; `Constants::FILENAME_INVALID_CHARS`) or
//!    any character from the admin-configured `forbidden_filename_characters` list → rejected
//! 6. Exact case-insensitive match against the `forbidden_filenames` list → rejected
//! 7. The basename (part up to the first non-leading `.`) is in `forbidden_filename_basenames` → rejected
//! 8. Ends with a suffix from `forbidden_filename_extensions` (case-insensitive) → rejected
//!    `.part` and `.filepart` are always in this list.

use std::sync::Arc;

use crate::config::NcConfig;

// ─── Error type ───────────────────────────────────────────────────────────────

/// Reason why a filename was rejected.
///
/// The `Display` impl produces human-readable messages suitable for use in
/// a DAV `<s:message>` XML element or a 400 response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilenameError {
    Empty,
    DotDirectory,
    TooLong,
    ControlChar,
    ForbiddenChar(char),
    ForbiddenName,
    ForbiddenBasename,
    ForbiddenExtension,
}

impl std::fmt::Display for FilenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "Filename is empty"),
            Self::DotDirectory => write!(f, "File name is a reserved directory name"),
            Self::TooLong => write!(f, "Filename too long (max 250 characters)"),
            Self::ControlChar => write!(f, "File name contains a control character"),
            Self::ForbiddenChar(c) => {
                write!(f, "File name contains forbidden character: {c:?}")
            }
            Self::ForbiddenName => write!(f, "File name is forbidden"),
            Self::ForbiddenBasename => write!(f, "File name prefix is forbidden"),
            Self::ForbiddenExtension => write!(f, "File type is forbidden"),
        }
    }
}

// ─── Validator ────────────────────────────────────────────────────────────────

/// Characters that are ALWAYS forbidden, regardless of config.
/// Mirrors PHP `Constants::FILENAME_INVALID_CHARS = '\\/'`.
const ALWAYS_FORBIDDEN_CHARS: &[char] = &['\\', '/'];

/// Statically built at startup from [`NcConfig`]; shared cheaply with `Arc`.
#[derive(Debug, Clone)]
pub struct FilenameValidator {
    /// Lower-cased exact filename matches (e.g. `[".htaccess"]`).
    forbidden_filenames: Vec<String>,
    /// Lower-cased basename prefixes (e.g. `["desktop.ini"]`).
    forbidden_basenames: Vec<String>,
    /// Characters forbidden at any position (always contains `\` and `/`).
    forbidden_characters: Vec<char>,
    /// Lower-cased extension suffixes (always contains `.part` and `.filepart`).
    forbidden_extensions: Vec<String>,
}

pub type SharedFilenameValidator = Arc<FilenameValidator>;

impl FilenameValidator {
    /// Build a validator from the parsed `config.php` values.
    pub fn from_config(config: &NcConfig) -> Self {
        // Characters: always-forbidden first, then admin additions.
        let mut forbidden_characters: Vec<char> = ALWAYS_FORBIDDEN_CHARS.to_vec();
        for s in &config.forbidden_filename_characters {
            for c in s.chars() {
                if !forbidden_characters.contains(&c) {
                    forbidden_characters.push(c);
                }
            }
        }

        // Extensions: config list + unconditionally required entries.
        let mut forbidden_extensions: Vec<String> = config
            .forbidden_filename_extensions
            .iter()
            .map(|s| s.to_lowercase())
            .collect();
        for always in [".part", ".filepart"] {
            if !forbidden_extensions.iter().any(|e| e == always) {
                forbidden_extensions.push(always.to_string());
            }
        }

        FilenameValidator {
            forbidden_filenames: config
                .forbidden_filenames
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            forbidden_basenames: config
                .forbidden_filename_basenames
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            forbidden_characters,
            forbidden_extensions,
        }
    }

    /// Validate a single filename (not a path — just the last component).
    ///
    /// Returns `Ok(())` when the name is acceptable, or `Err(FilenameError)`
    /// with the first reason the name was rejected.
    pub fn validate(&self, filename: &str) -> Result<(), FilenameError> {
        // 1. Empty / whitespace-only
        if filename.trim().is_empty() {
            return Err(FilenameError::Empty);
        }

        // 2. Reserved directory names '.' and '..'
        if filename == "." || filename == ".." {
            return Err(FilenameError::DotDirectory);
        }

        // 3. Length limit: oc_filecache.name is VARCHAR(250)
        if filename.len() > 250 {
            return Err(FilenameError::TooLong);
        }

        // 4. Control characters (bytes 0–31)
        if filename.bytes().any(|b| b < 32) {
            return Err(FilenameError::ControlChar);
        }

        // 5. Forbidden characters (\ / and admin-configured)
        for &c in &self.forbidden_characters {
            if filename.contains(c) {
                return Err(FilenameError::ForbiddenChar(c));
            }
        }

        let filename_lc = filename.to_lowercase();

        // 6. Exact forbidden filename match (case-insensitive)
        if self.forbidden_filenames.contains(&filename_lc) {
            return Err(FilenameError::ForbiddenName);
        }

        // 7. Forbidden basename check.
        //    Basename = everything up to the first '.' that is NOT the leading char.
        //    Mirrors PHP: `substr($filename, 0, strpos($filename, '.', 1) ?: null)`
        if !self.forbidden_basenames.is_empty() {
            let first_mid_dot = filename_lc[1..].find('.').map(|i| i + 1);
            let basename = match first_mid_dot {
                Some(pos) => &filename_lc[..pos],
                None => &filename_lc[..],
            };
            if !basename.is_empty() && self.forbidden_basenames.iter().any(|b| b == basename) {
                return Err(FilenameError::ForbiddenBasename);
            }
        }

        // 8. Forbidden extension (suffix match, case-insensitive)
        for ext in &self.forbidden_extensions {
            if filename_lc.ends_with(ext.as_str()) {
                return Err(FilenameError::ForbiddenExtension);
            }
        }

        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NcConfig;

    fn default_validator() -> FilenameValidator {
        let cfg = NcConfig::from_php_config("<?php\n$CONFIG = ['dbtype' => 'sqlite3'];").unwrap();
        FilenameValidator::from_config(&cfg)
    }

    // ── Basic acceptance ──────────────────────────────────────────────────────

    #[test]
    fn plain_name_accepted() {
        assert!(default_validator().validate("hello.txt").is_ok());
    }

    #[test]
    fn hidden_file_accepted() {
        assert!(default_validator().validate(".bashrc").is_ok());
    }

    #[test]
    fn unicode_name_accepted() {
        assert!(default_validator().validate("données.csv").is_ok());
    }

    // ── Rule 1: empty ─────────────────────────────────────────────────────────

    #[test]
    fn empty_string_rejected() {
        assert_eq!(default_validator().validate(""), Err(FilenameError::Empty));
    }

    #[test]
    fn whitespace_only_rejected() {
        assert_eq!(
            default_validator().validate("   "),
            Err(FilenameError::Empty)
        );
    }

    // ── Rule 2: dot directories ───────────────────────────────────────────────

    #[test]
    fn single_dot_rejected() {
        assert_eq!(
            default_validator().validate("."),
            Err(FilenameError::DotDirectory)
        );
    }

    #[test]
    fn double_dot_rejected() {
        assert_eq!(
            default_validator().validate(".."),
            Err(FilenameError::DotDirectory)
        );
    }

    // ── Rule 3: length ────────────────────────────────────────────────────────

    #[test]
    fn exactly_250_chars_accepted() {
        let name = "a".repeat(250);
        assert!(default_validator().validate(&name).is_ok());
    }

    #[test]
    fn name_251_chars_rejected() {
        let name = "a".repeat(251);
        assert_eq!(
            default_validator().validate(&name),
            Err(FilenameError::TooLong)
        );
    }

    // ── Rule 4: control characters ────────────────────────────────────────────

    #[test]
    fn null_byte_rejected() {
        assert_eq!(
            default_validator().validate("file\x00name"),
            Err(FilenameError::ControlChar)
        );
    }

    #[test]
    fn newline_rejected() {
        assert_eq!(
            default_validator().validate("file\nname"),
            Err(FilenameError::ControlChar)
        );
    }

    // ── Rule 5: forbidden characters ─────────────────────────────────────────

    #[test]
    fn backslash_rejected() {
        assert_eq!(
            default_validator().validate("path\\name"),
            Err(FilenameError::ForbiddenChar('\\'))
        );
    }

    #[test]
    fn forward_slash_rejected() {
        assert_eq!(
            default_validator().validate("path/name"),
            Err(FilenameError::ForbiddenChar('/'))
        );
    }

    // ── Rule 6: forbidden filenames ───────────────────────────────────────────

    #[test]
    fn htaccess_rejected() {
        assert_eq!(
            default_validator().validate(".htaccess"),
            Err(FilenameError::ForbiddenName)
        );
    }

    #[test]
    fn htaccess_uppercase_rejected() {
        assert_eq!(
            default_validator().validate(".HTACCESS"),
            Err(FilenameError::ForbiddenName)
        );
    }

    #[test]
    fn htaccess_with_extension_accepted() {
        // ".htaccess.bak" is NOT the same as ".htaccess" — forbidden name is exact
        assert!(default_validator().validate(".htaccess.bak").is_ok());
    }

    // ── Rule 7: forbidden basenames ───────────────────────────────────────────

    #[test]
    fn forbidden_basename_with_extension_rejected() {
        let cfg = NcConfig::from_php_config(
            "<?php\n$CONFIG = ['dbtype' => 'sqlite3', 'forbidden_filename_basenames' => ['Thumbs']];",
        )
        .unwrap();
        let v = FilenameValidator::from_config(&cfg);
        // "Thumbs.db" → basename "thumbs" (lowercase) → forbidden
        assert_eq!(
            v.validate("Thumbs.db"),
            Err(FilenameError::ForbiddenBasename)
        );
    }

    #[test]
    fn forbidden_basename_exact_match_also_rejected() {
        let cfg = NcConfig::from_php_config(
            "<?php\n$CONFIG = ['dbtype' => 'sqlite3', 'forbidden_filename_basenames' => ['Thumbs']];",
        )
        .unwrap();
        let v = FilenameValidator::from_config(&cfg);
        // "Thumbs" alone → basename "thumbs" → forbidden
        assert_eq!(v.validate("Thumbs"), Err(FilenameError::ForbiddenBasename));
    }

    #[test]
    fn hidden_file_with_forbidden_base_rejected() {
        // ".htpasswd" — the PHP checks with `strpos($filename, '.', 1)`.
        // Leading dot is skipped; no second dot → the whole string is the "basename".
        // This sits at rule 6 (forbidden name) not rule 7 unless ".htpasswd" is in
        // the basenames list.  Here we test rule 7 directly with a configured basename.
        let cfg = NcConfig::from_php_config(
            "<?php\n$CONFIG = ['dbtype' => 'sqlite3', 'forbidden_filename_basenames' => ['.htpasswd']];",
        )
        .unwrap();
        let v = FilenameValidator::from_config(&cfg);
        assert_eq!(
            v.validate(".htpasswd"),
            Err(FilenameError::ForbiddenBasename)
        );
    }

    // ── Rule 8: forbidden extensions ─────────────────────────────────────────

    #[test]
    fn part_extension_rejected() {
        assert_eq!(
            default_validator().validate("upload.part"),
            Err(FilenameError::ForbiddenExtension)
        );
    }

    #[test]
    fn filepart_extension_rejected() {
        assert_eq!(
            default_validator().validate("upload.filepart"),
            Err(FilenameError::ForbiddenExtension)
        );
    }

    #[test]
    fn custom_extension_rejected() {
        let cfg = NcConfig::from_php_config(
            "<?php\n$CONFIG = ['dbtype' => 'sqlite3', 'forbidden_filename_extensions' => ['.exe']];",
        )
        .unwrap();
        let v = FilenameValidator::from_config(&cfg);
        assert_eq!(
            v.validate("malware.exe"),
            Err(FilenameError::ForbiddenExtension)
        );
        // Still has the built-in ones too
        assert_eq!(
            v.validate("upload.part"),
            Err(FilenameError::ForbiddenExtension)
        );
    }

    #[test]
    fn extension_case_insensitive() {
        assert_eq!(
            default_validator().validate("upload.PART"),
            Err(FilenameError::ForbiddenExtension)
        );
    }
}
