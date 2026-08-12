//! Imaginary pipeline construction — output-format resolution, quality selection,
//! and the `operations` query-parameter JSON (Phase 11.4).
//!
//! Everything here is **pure** and verified against golden vectors captured from
//! live PHP: the `operations` JSON must serialize **byte-identical** to PHP's
//! `json_encode` (`Imaginary.php:106-142`), because Imaginary's behaviour is driven
//! by that string and any drift is a silent output-format/quality mismatch.  Field
//! order is preserved by serde's derive (declaration order), matching PHP's
//! associative-array insertion order; `quality` is carried as a **string** (PHP
//! `getAppValue` returns strings) and `width`/`height` as integers.
//!
//! ## Pipeline shape (PHP `Imaginary::getCroppedThumbnail`)
//!
//! - **op1** (from the *raw source* only): `convert{type}` for `svg+xml`/`pdf`/
//!   `illustrator`; `autorotate` for most; **neither** for `heic` (autorotate is
//!   broken for HEIC).  The `convert` `type` is the *final* output format (so the
//!   one-way `preview_format=webp` override applies to it too).
//! - **op2** (always): `fit` (crop=false) or `smartcrop` (crop=true) with
//!   `{width, height, stripmeta:"true", type:<final format>, norotation:"true",
//!   quality:<string>}`.
//!
//! ## Max vs derived (the max-first model, §11.4)
//!
//! The **max preview** is generated from the raw source with op1 + `fit` — always
//! `crop=false`, because PHP's `generateProviderPreview` calls the provider via
//! `getThumbnail` → `getCroppedThumbnail(crop=false)`.  A **derived** variant is
//! produced by re-submitting the *already-sanitized* max-preview bytes (autorotated,
//! converted, metadata-stripped) — so its pipeline is **op2 only** (no op1; re-running
//! `convert` on already-converted bytes would be wrong).  This is
//! [`derive_operations_json`].

use serde::Serialize;

/// Imaginary's output format — the short `type` parameter value and the full output
/// mimetype (recorded in `oc_previews.mimetype` and served as `Content-Type`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputFormat {
    Jpeg,
    Png,
    Webp,
}

impl OutputFormat {
    /// Imaginary's short `type` parameter (`jpeg`/`png`/`webp`).
    pub fn short(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Webp => "webp",
        }
    }

    /// The full output mimetype (`image/jpeg`/`image/png`/`image/webp`).
    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }

    /// The inverse of [`Self::mime`] — resolve a max preview's stored output mimetype
    /// back to its format, to derive a smaller variant in the same format.
    pub fn from_mime(mime: &str) -> Option<OutputFormat> {
        match mime {
            "image/jpeg" => Some(Self::Jpeg),
            "image/png" => Some(Self::Png),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }
}

/// PHP `Imaginary.php:72-104` output-format resolution: source-mime map — `gif`/
/// `png`/`svg+xml`/`pdf`/`illustrator` → `png`; everything else (`jpeg`/`bmp`/
/// `x-bitmap`/`tiff`/`webp`/`heif`/`heic`/unknown) → `jpeg` — then the **one-way**
/// `preview_format=webp` override (any other value, incl. the default `jpeg`, leaves
/// the mapping untouched).
pub fn output_format(source_mime: &str, preview_format: Option<&str>) -> OutputFormat {
    let base = match source_mime {
        "image/gif"
        | "image/png"
        | "image/svg+xml"
        | "application/pdf"
        | "application/illustrator" => OutputFormat::Png,
        _ => OutputFormat::Jpeg,
    };
    match preview_format {
        Some("webp") => OutputFormat::Webp,
        _ => base,
    }
}

/// PHP `Imaginary.php:121-130` quality selection: `webp` output reads
/// `preview/webp_quality`; `jpeg` **and** `png` output read `preview/jpeg_quality`
/// (png falls into the `default` branch).  Each defaults to `"80"`.  Returned as a
/// string — PHP `getAppValue` returns strings and the operations JSON carries the
/// value verbatim.
pub fn quality_for(format: OutputFormat, jpeg_quality: &str, webp_quality: &str) -> String {
    match format {
        OutputFormat::Webp => webp_quality.to_string(),
        OutputFormat::Jpeg | OutputFormat::Png => jpeg_quality.to_string(),
    }
}

// ─── operations JSON (serde structs → byte-identical to PHP json_encode) ────────

#[derive(Serialize)]
struct ResizeParams {
    width: u32,
    height: u32,
    stripmeta: &'static str,
    #[serde(rename = "type")]
    typ: &'static str,
    norotation: &'static str,
    quality: String,
}

#[derive(Serialize)]
struct ConvertParams {
    #[serde(rename = "type")]
    typ: &'static str,
}

/// One Imaginary pipeline operation.  Internally tagged so the serialized key order
/// is `operation` first (matching PHP's `['operation' => …, 'params' => …]`).
#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "lowercase")]
enum Op {
    Autorotate,
    Convert { params: ConvertParams },
    Fit { params: ResizeParams },
    Smartcrop { params: ResizeParams },
}

/// op1 for the **raw source** (`Imaginary.php:75-119`): `convert{type}` (svg/pdf/
/// illustrator — the `type` is the final, possibly-webp-overridden format), `autorotate`
/// (most), or `None` (heic — autorotate disabled).
fn op1_for_source(source_mime: &str, out: OutputFormat) -> Option<Op> {
    match source_mime {
        "image/svg+xml" | "application/pdf" | "application/illustrator" => Some(Op::Convert {
            params: ConvertParams { typ: out.short() },
        }),
        "image/heic" => None,
        _ => Some(Op::Autorotate),
    }
}

/// op2 (`Imaginary.php:132-142`): `fit` (crop=false) or `smartcrop` (crop=true).
fn op2(w: u32, h: u32, crop: bool, out: OutputFormat, quality: String) -> Op {
    let params = ResizeParams {
        width: w,
        height: h,
        stripmeta: "true",
        typ: out.short(),
        norotation: "true",
        quality,
    };
    if crop {
        Op::Smartcrop { params }
    } else {
        Op::Fit { params }
    }
}

fn to_json(ops: &[Op]) -> String {
    serde_json::to_string(ops).expect("operations serialize")
}

/// The **max-preview** pipeline (raw source → Imaginary): op1 (per source mime) +
/// `fit` at the clamped max dims.  Always `crop=false` (PHP's generator requests the
/// max via `getThumbnail`).  `preview_format`/`jpeg_quality`/`webp_quality` are the
/// resolved config/appconfig values.
pub fn max_operations_json(
    source_mime: &str,
    max_w: u32,
    max_h: u32,
    preview_format: Option<&str>,
    jpeg_quality: &str,
    webp_quality: &str,
) -> String {
    let out = output_format(source_mime, preview_format);
    let quality = quality_for(out, jpeg_quality, webp_quality);
    let mut ops = Vec::with_capacity(2);
    if let Some(o1) = op1_for_source(source_mime, out) {
        ops.push(o1);
    }
    ops.push(op2(max_w, max_h, false, out, quality));
    to_json(&ops)
}

/// The **derived-variant** pipeline (already-sanitized max-preview bytes → Imaginary):
/// **op2 only** — `fit` (crop=false) or `smartcrop` (crop=true) — at the bucketed
/// size, in the max preview's output `format`.  No op1: the max bytes are already
/// autorotated/converted/metadata-stripped.
pub fn derive_operations_json(
    w: u32,
    h: u32,
    crop: bool,
    format: OutputFormat,
    quality: &str,
) -> String {
    to_json(&[op2(w, h, crop, format, quality.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── output format resolution ───────────────────────────────────────────

    #[test]
    fn output_format_source_map() {
        // gif/png/svg/pdf/illustrator → png; everything else → jpeg.
        assert_eq!(output_format("image/png", None), OutputFormat::Png);
        assert_eq!(output_format("image/gif", None), OutputFormat::Png);
        assert_eq!(output_format("image/svg+xml", None), OutputFormat::Png);
        assert_eq!(output_format("application/pdf", None), OutputFormat::Png);
        assert_eq!(
            output_format("application/illustrator", None),
            OutputFormat::Png
        );
        for m in [
            "image/jpeg",
            "image/bmp",
            "image/x-bitmap",
            "image/tiff",
            "image/webp",
            "image/heif",
            "image/heic",
            "application/octet-stream", // unknown → default jpeg
        ] {
            assert_eq!(output_format(m, None), OutputFormat::Jpeg, "{m}");
        }
    }

    #[test]
    fn output_format_webp_override_is_one_way() {
        // Only "webp" overrides; any other value (incl. the default "jpeg") is a no-op.
        assert_eq!(output_format("image/png", Some("webp")), OutputFormat::Webp);
        assert_eq!(
            output_format("image/heic", Some("webp")),
            OutputFormat::Webp
        );
        assert_eq!(output_format("image/png", Some("jpeg")), OutputFormat::Png);
        assert_eq!(
            output_format("image/jpeg", Some("jpeg")),
            OutputFormat::Jpeg
        );
        assert_eq!(output_format("image/png", Some("gif")), OutputFormat::Png); // not webp → no-op
        assert_eq!(output_format("image/png", None), OutputFormat::Png);
    }

    #[test]
    fn quality_selection() {
        // jpeg + png → jpeg_quality; webp → webp_quality.
        assert_eq!(quality_for(OutputFormat::Jpeg, "75", "60"), "75");
        assert_eq!(quality_for(OutputFormat::Png, "75", "60"), "75");
        assert_eq!(quality_for(OutputFormat::Webp, "75", "60"), "60");
    }

    #[test]
    fn format_mime_roundtrip() {
        for f in [OutputFormat::Jpeg, OutputFormat::Png, OutputFormat::Webp] {
            assert_eq!(OutputFormat::from_mime(f.mime()), Some(f));
        }
        assert_eq!(OutputFormat::from_mime("image/gif"), None);
    }

    // ── operations JSON — golden vectors captured from live PHP ────────────
    // Each expected string is the verbatim `json_encode` output of the replicated
    // `Imaginary.php:72-142` builder (probe in the phase-11 work log).  Byte-identical
    // matching guards against any field-order / type / quoting drift.

    /// `(source_mime, expected)` for a max-preview generation at 4096×4096,
    /// default `preview_format`, quality 80/80.
    const MAX_GOLDEN: &[(&str, &str)] = &[
        (
            "image/bmp",
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#,
        ),
        (
            "image/x-bitmap",
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#,
        ),
        (
            "image/png",
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"png","norotation":"true","quality":"80"}}]"#,
        ),
        (
            "image/jpeg",
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#,
        ),
        (
            "image/gif",
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"png","norotation":"true","quality":"80"}}]"#,
        ),
        // heic: NEITHER autorotate nor convert.
        (
            "image/heic",
            r#"[{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#,
        ),
        (
            "image/heif",
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#,
        ),
        // svg+xml / illustrator / pdf: convert{type:png} (no autorotate).
        (
            "image/svg+xml",
            r#"[{"operation":"convert","params":{"type":"png"}},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"png","norotation":"true","quality":"80"}}]"#,
        ),
        (
            "image/tiff",
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#,
        ),
        (
            "image/webp",
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#,
        ),
        (
            "application/illustrator",
            r#"[{"operation":"convert","params":{"type":"png"}},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"png","norotation":"true","quality":"80"}}]"#,
        ),
        (
            "application/pdf",
            r#"[{"operation":"convert","params":{"type":"png"}},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"png","norotation":"true","quality":"80"}}]"#,
        ),
    ];

    #[test]
    fn imaginary_pipeline_per_mimetype() {
        for &(mime, expected) in MAX_GOLDEN {
            assert_eq!(
                max_operations_json(mime, 4096, 4096, None, "80", "80"),
                expected,
                "max pipeline for {mime}"
            );
        }
    }

    #[test]
    fn webp_override_in_pipeline() {
        // The override flows into BOTH the convert `type` (svg) and op2 `type`.
        assert_eq!(
            max_operations_json("image/jpeg", 4096, 4096, Some("webp"), "80", "80"),
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"webp","norotation":"true","quality":"80"}}]"#
        );
        assert_eq!(
            max_operations_json("image/png", 4096, 4096, Some("webp"), "80", "80"),
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"webp","norotation":"true","quality":"80"}}]"#
        );
        // heic: still no op1, but webp output.
        assert_eq!(
            max_operations_json("image/heic", 4096, 4096, Some("webp"), "80", "80"),
            r#"[{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"webp","norotation":"true","quality":"80"}}]"#
        );
        // svg: convert{type:webp}.
        assert_eq!(
            max_operations_json("image/svg+xml", 4096, 4096, Some("webp"), "80", "80"),
            r#"[{"operation":"convert","params":{"type":"webp"}},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"webp","norotation":"true","quality":"80"}}]"#
        );
        assert_eq!(
            max_operations_json("image/gif", 4096, 4096, Some("webp"), "80", "80"),
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":4096,"height":4096,"stripmeta":"true","type":"webp","norotation":"true","quality":"80"}}]"#
        );
    }

    #[test]
    fn quality_passthrough_in_pipeline() {
        // jpeg_quality reaches the jpeg pipeline…
        assert_eq!(
            max_operations_json("image/jpeg", 1024, 768, None, "75", "80"),
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":1024,"height":768,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"75"}}]"#
        );
        // …and webp_quality reaches the webp pipeline (webp source + webp override).
        assert_eq!(
            max_operations_json("image/webp", 512, 512, Some("webp"), "80", "60"),
            r#"[{"operation":"autorotate"},{"operation":"fit","params":{"width":512,"height":512,"stripmeta":"true","type":"webp","norotation":"true","quality":"60"}}]"#
        );
    }

    #[test]
    fn derive_pipeline_is_op2_only() {
        // Derived variants re-submit already-sanitized max bytes → op2 ONLY (no op1).
        // fit (crop=false):
        assert_eq!(
            derive_operations_json(256, 171, false, OutputFormat::Jpeg, "80"),
            r#"[{"operation":"fit","params":{"width":256,"height":171,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#
        );
        // smartcrop (crop=true):
        assert_eq!(
            derive_operations_json(256, 256, true, OutputFormat::Jpeg, "80"),
            r#"[{"operation":"smartcrop","params":{"width":256,"height":256,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#
        );
        // png / webp formats:
        assert_eq!(
            derive_operations_json(256, 256, true, OutputFormat::Png, "80"),
            r#"[{"operation":"smartcrop","params":{"width":256,"height":256,"stripmeta":"true","type":"png","norotation":"true","quality":"80"}}]"#
        );
        assert_eq!(
            derive_operations_json(128, 128, false, OutputFormat::Webp, "60"),
            r#"[{"operation":"fit","params":{"width":128,"height":128,"stripmeta":"true","type":"webp","norotation":"true","quality":"60"}}]"#
        );
    }

    // A full from-source crop pipeline (op1 + smartcrop) is what PHP's
    // `getCroppedThumbnail(crop=true)` emits — captured for reference; on PHP's
    // Generator path it is unreachable (the max is always requested with crop=false),
    // and Rust's derive step uses op2-only (above).  Kept to document the exact PHP
    // shape and pin the smartcrop serialization.
    #[test]
    fn php_full_crop_pipeline_shape_reference() {
        // Reproduce the PHP op1+smartcrop shape via the internal builders.
        let out = output_format("image/jpeg", None);
        let ops = vec![
            op1_for_source("image/jpeg", out).unwrap(),
            op2(256, 256, true, out, "80".to_string()),
        ];
        assert_eq!(
            to_json(&ops),
            r#"[{"operation":"autorotate"},{"operation":"smartcrop","params":{"width":256,"height":256,"stripmeta":"true","type":"jpeg","norotation":"true","quality":"80"}}]"#
        );
    }
}
