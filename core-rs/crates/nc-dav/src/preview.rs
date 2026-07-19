//! Preview availability detection for `{nc:}has-preview` (§10.12 / §11.1).
//!
//! Mirrors PHP `PreviewManager::isAvailable()` which checks four layers:
//! 1. `enable_previews` system config
//! 2. Mount-point `previews` option (per-external-storage; home storage always on)
//! 3. Mimetype match against registered provider patterns
//! 4. Per-provider binary availability (ffmpeg, LibreOffice, ImageMagick)

/// Returns `true` when a preview *could* be generated for a file with the
/// given mimetype, given the current server config.
///
/// This tells the web UI whether to request a thumbnail — it does NOT
/// guarantee that generation will succeed (the backend may still fail
/// for resource or format-specific reasons at generation time).
///
/// ## Checked layers (matches PHP `PreviewManager::isAvailable()`)
///
/// | Layer | PHP | Rust |
/// |-------|-----|------|
/// | `enable_previews` | `config->getSystemValueBool('enable_previews', true)` | `NcConfig::enable_previews` (default `true`) |
/// | Mount-point `previews` | `$mount->getOption('previews', true)` — per-external-storage toggle | `is_mounted` gating — home storage always `true`; external storage TODO |
/// | Mimetype match | Provider regexes (`/image\/png/`, `/video\//`, …) | `match` + `starts_with()` catch-alls |
/// | Per-provider binary | `isAvailable()` → binary config / imagetypes | ffmpeg / LibreOffice config keys; HEIC → `false` |
///
/// ## Provider mapping (matches PHP `registerCoreProviders`)
///
/// | Category | Mimetypes | Gate |
/// |----------|----------|------|
/// | Images | `image/png`, `image/jpeg`, `image/gif`, `image/bmp`, `image/webp`, `image/svg+xml`, `image/tiff`, `image/x-xbitmap`, generic `image/*` | `enable_previews` only |
/// | HEIC/HEIF | `image/heic`, `image/heif` | Disabled (requires ImageMagick HEIC delegate) |
/// | Video | `video/*` | `enable_previews` + `preview_ffmpeg_path` |
/// | Audio | `audio/mpeg`, `audio/mp3` | `enable_previews` only |
/// | Text | `text/plain`, `text/markdown`, `text/x-markdown` | `enable_previews` only |
/// | PDF | `application/pdf` | `enable_previews` only |
/// | Office | `application/msword`, `application/vnd.ms-*`, `application/vnd.openxmlformats-officedocument.*`, `application/vnd.oasis.opendocument.*` | `enable_previews` + `preview_libreoffice_path` |
/// | Font | `font/*` | `enable_previews` only |
/// | Other | `application/postscript`, `application/illustrator`, `application/x-krita` | `enable_previews` only |
pub fn has_preview(
    mime_type: &str,
    enable_previews: bool,
    is_mounted: bool,
    preview_ffmpeg_path: Option<&str>,
    preview_libreoffice_path: Option<&str>,
) -> bool {
    if !enable_previews {
        return false;
    }

    // Mount-point previews toggle.  Home storage has no mount point so
    // this is always enabled.  External storages can disable previews
    // via mount options — TODO when external storage support lands.
    // PHP: $mount && !$mount->getOption('previews', true) → false.
    // The PHP default for the option is `true`, so a missing mount or
    // missing option means previews are allowed.
    if is_mounted {
        // External storage: `mount->getOption('previews', true)` — defaults
        // to true (previews allowed).  When mount options are wired, check
        // the config here; for now assume true matching PHP's default.
    }

    // Fast path: strip charset suffix commonly appended to mimetypes
    // (e.g. "text/plain; charset=utf-8" → "text/plain").
    let mime = mime_type.split(';').next().unwrap_or(mime_type).trim();

    match mime {
        // ── Image providers — always available (ProviderV2) ──────────────
        "image/png" | "image/jpeg" | "image/gif" | "image/bmp"
        | "image/webp" | "image/svg+xml" | "image/tiff"
        | "image/x-xbitmap" | "image/x-photoshop" => true,

        // HEIC/HEIF — requires ImageMagick HEIC delegate; unavailable
        // unless proven otherwise.
        "image/heic" | "image/heif" | "image/heic-sequence"
        | "image/heif-sequence" => false,

        // Catch-all for any other image/* type
        _ if mime.starts_with("image/") => true,

        // ── Video — requires ffmpeg binary ───────────────────────────────
        _ if mime.starts_with("video/") => {
            preview_ffmpeg_path.map_or(false, |p| !p.is_empty())
        }

        // ── Audio — specific supported formats ───────────────────────────
        "audio/mpeg" | "audio/mp3" => true,

        // ── Text ─────────────────────────────────────────────────────────
        "text/plain" | "text/markdown" | "text/x-markdown" => true,

        // ── PDF ──────────────────────────────────────────────────────────
        "application/pdf" => true,

        // ── Office documents — requires LibreOffice ──────────────────────
        "application/msword"
        | "application/vnd.ms-word"
        | "application/vnd.ms-excel"
        | "application/vnd.ms-powerpoint" => {
            preview_libreoffice_path.map_or(false, |p| !p.is_empty())
        }

        // Office XML / OpenDocument
        _ if mime.starts_with("application/vnd.openxmlformats-officedocument.")
            || mime.starts_with("application/vnd.oasis.opendocument.")
            || mime.starts_with("application/vnd.ms-") => {
            preview_libreoffice_path.map_or(false, |p| !p.is_empty())
        }

        // ── Font, Postscript, Illustrator, Krita ─────────────────────────
        "application/postscript" | "application/illustrator"
        | "application/x-krita" | "application/x-font" => true,

        // Catch font/*
        _ if mime.starts_with("font/") => true,

        // ── Everything else: no preview provider ─────────────────────────
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_for<'a>(
        enable: bool,
        ffmpeg: Option<&'a str>,
        libreoffice: Option<&'a str>,
    ) -> (bool, bool, Option<&'a str>, Option<&'a str>) {
        (enable, false, ffmpeg, libreoffice)
    }

    // ── enable_previews gate ─────────────────────────────────────────────

    #[test]
    fn disabled_always_false() {
        let c = cfg_for(false, Some("/usr/bin/ffmpeg"), Some("/usr/bin/soffice"));
        assert!(!has_preview("image/png", c.0, c.1, c.2, c.3));
        assert!(!has_preview("image/jpeg", c.0, c.1, c.2, c.3));
        assert!(!has_preview("video/mp4", c.0, c.1, c.2, c.3));
        assert!(!has_preview("text/plain", c.0, c.1, c.2, c.3));
    }

    // ── Image types ──────────────────────────────────────────────────────

    #[test]
    fn image_types_always_available() {
        let c = cfg_for(true, None, None);
        assert!(has_preview("image/png", c.0, c.1, c.2, c.3));
        assert!(has_preview("image/jpeg", c.0, c.1, c.2, c.3));
        assert!(has_preview("image/gif", c.0, c.1, c.2, c.3));
        assert!(has_preview("image/bmp", c.0, c.1, c.2, c.3));
        assert!(has_preview("image/webp", c.0, c.1, c.2, c.3));
        assert!(has_preview("image/svg+xml", c.0, c.1, c.2, c.3));
        assert!(has_preview("image/tiff", c.0, c.1, c.2, c.3));
    }

    #[test]
    fn generic_image_type_available() {
        let c = cfg_for(true, None, None);
        // Not in the explicit list but starts with "image/"
        assert!(has_preview("image/avif", c.0, c.1, c.2, c.3));
    }

    #[test]
    fn heic_unavailable() {
        let c = cfg_for(true, None, None);
        assert!(!has_preview("image/heic", c.0, c.1, c.2, c.3));
        assert!(!has_preview("image/heif", c.0, c.1, c.2, c.3));
    }

    // ── Video types ──────────────────────────────────────────────────────

    #[test]
    fn video_with_ffmpeg() {
        let c = cfg_for(true, Some("/usr/bin/ffmpeg"), None);
        assert!(has_preview("video/mp4", c.0, c.1, c.2, c.3));
        assert!(has_preview("video/webm", c.0, c.1, c.2, c.3));
    }

    #[test]
    fn video_without_ffmpeg() {
        let c = cfg_for(true, None, None);
        assert!(!has_preview("video/mp4", c.0, c.1, c.2, c.3));
        assert!(!has_preview("video/quicktime", c.0, c.1, c.2, c.3));
    }

    #[test]
    fn video_with_empty_ffmpeg_path() {
        let c = cfg_for(true, Some(""), None);
        assert!(!has_preview("video/mp4", c.0, c.1, c.2, c.3));
    }

    // ── Audio types ──────────────────────────────────────────────────────

    #[test]
    fn audio_mp3_available() {
        let c = cfg_for(true, None, None);
        assert!(has_preview("audio/mpeg", c.0, c.1, c.2, c.3));
    }

    #[test]
    fn audio_other_not_available() {
        let c = cfg_for(true, None, None);
        assert!(!has_preview("audio/ogg", c.0, c.1, c.2, c.3));
        assert!(!has_preview("audio/flac", c.0, c.1, c.2, c.3));
    }

    // ── Text types ───────────────────────────────────────────────────────

    #[test]
    fn text_types_available() {
        let c = cfg_for(true, None, None);
        assert!(has_preview("text/plain", c.0, c.1, c.2, c.3));
        assert!(has_preview("text/markdown", c.0, c.1, c.2, c.3));
    }

    // ── PDF ──────────────────────────────────────────────────────────────

    #[test]
    fn pdf_available() {
        let c = cfg_for(true, None, None);
        assert!(has_preview("application/pdf", c.0, c.1, c.2, c.3));
    }

    // ── Office types ─────────────────────────────────────────────────────

    #[test]
    fn office_with_libreoffice() {
        let c = cfg_for(true, None, Some("/usr/bin/soffice"));
        assert!(has_preview("application/msword", c.0, c.1, c.2, c.3));
        assert!(has_preview(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            c.0, c.1, c.2, c.3
        ));
        assert!(has_preview(
            "application/vnd.oasis.opendocument.text",
            c.0, c.1, c.2, c.3
        ));
    }

    #[test]
    fn office_without_libreoffice() {
        let c = cfg_for(true, None, None);
        assert!(!has_preview("application/msword", c.0, c.1, c.2, c.3));
        assert!(!has_preview(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            c.0, c.1, c.2, c.3
        ));
    }

    // ── Font types ───────────────────────────────────────────────────────

    #[test]
    fn font_types_available() {
        let c = cfg_for(true, None, None);
        assert!(has_preview("font/ttf", c.0, c.1, c.2, c.3));
        assert!(has_preview("font/woff2", c.0, c.1, c.2, c.3));
    }

    // ── Unsupported types ────────────────────────────────────────────────

    #[test]
    fn unknown_type_not_available() {
        let c = cfg_for(true, Some("/usr/bin/ffmpeg"), Some("/usr/bin/soffice"));
        assert!(!has_preview("application/octet-stream", c.0, c.1, c.2, c.3));
        assert!(!has_preview("application/zip", c.0, c.1, c.2, c.3));
        assert!(!has_preview("model/obj", c.0, c.1, c.2, c.3));
    }

    // ── Charset stripping ────────────────────────────────────────────────

    #[test]
    fn stripe_charset_suffix() {
        let c = cfg_for(true, None, None);
        assert!(has_preview("text/plain; charset=utf-8", c.0, c.1, c.2, c.3));
        assert!(has_preview("image/png; charset=binary", c.0, c.1, c.2, c.3));
    }
}
