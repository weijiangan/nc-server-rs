//! Size negotiation — a PHP-exact port of `Generator::calculateSize`
//! (`lib/private/Preview/Generator.php:420-496`).
//!
//! Given a requested `(width, height)` and the **max preview's actual pixel
//! dimensions**, this returns the bucketed size PHP would look up in `oc_previews`
//! or generate.  Byte-for-byte bucketing parity is correctness-critical: if Rust
//! buckets differently from PHP, each side writes rows the other never looks for
//! and the cache-hit rate collapses in both directions during coexistence.
//!
//! ## Algorithm (mirrors PHP step for step)
//!
//! 1. **Aspect folding** (`!crop` only): resolve `-1` dimensions against the source
//!    ratio, then fold the fill/cover mode into the dimensions — *fill* treats the
//!    request as the outer box (fit inside), *cover* as the inner box (cover it).
//! 2. **Power-of-4 snap**: snap each dimension **up** to the nearest power of 4
//!    (minimum 64), scaling the other axis proportionally.  Skipped when a
//!    dimension already equals the max preview (the request *is* the max preview).
//! 3. **Clamp** into the max preview, preserving aspect.
//! 4. **Round** half-away-from-zero (PHP `round`) to integer pixels.
//!
//! ## Caller-side rules (NOT in this function — `generatePreviews`, `:151-172`)
//!
//! - `width == -1 && height == -1` → the max preview is served as-is.
//! - result `== (max_width, max_height)` → the max preview row is served.
//!
//! ## The strict-`!==` subtlety (faithfully replicated)
//!
//! PHP skips the power-of-4 step under `$height !== $maxHeight && $width !==
//! $maxWidth`.  `!==` is **type-strict**: a dimension that was reassigned to a
//! *float* never equals the *integer* max, even when numerically identical.  We
//! track `w_is_raw` / `h_is_raw` ("never reassigned") so the skip fires exactly
//! when PHP's strict comparison would — a dimension still holding its original
//! integer value equal to the max.

/// Preview scaling mode (PHP `IPreview::MODE_FILL` / `IPreview::MODE_COVER`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Requested `width`/`height` are the **outer** box — the result fits inside.
    Fill,
    /// Requested `width`/`height` are the **inner** box — the result covers them.
    Cover,
}

impl Mode {
    /// Parse PHP's `mode` query parameter (`"fill"` / `"cover"`).  PHP defaults any
    /// other value to `MODE_FILL` (`$specification['mode'] ?? IPreview::MODE_FILL`).
    pub fn from_php(mode: &str) -> Self {
        match mode {
            "cover" => Mode::Cover,
            _ => Mode::Fill,
        }
    }
}

/// PHP-exact `Generator::calculateSize`.
///
/// * `width`, `height` — requested dimensions; `-1` means "derive from the source
///   aspect ratio" (only meaningful when `!crop`).
/// * `crop` — `true` skips aspect folding entirely (requested box taken literally).
/// * `mode` — fill/cover aspect folding (ignored when `crop`).
/// * `max_width`, `max_height` — the **max preview's actual pixel dimensions** (not
///   the configured `preview_max_x`/`preview_max_y`), per `generatePreviews`
///   (`Generator.php:142-143`).
///
/// Returns the bucketed `(width, height)`.
///
/// Degenerate input note: `crop == true` with a `-1` dimension is nonsensical (PHP
/// computes `log(-1) = NAN` and yields `0`); callers never produce it — a `-1`
/// dimension implies preserve-aspect, i.e. `crop == false`.  We mirror PHP's
/// NaN→0 behaviour rather than special-casing it.
pub fn calculate_size(
    width: i64,
    height: i64,
    crop: bool,
    mode: Mode,
    max_width: i64,
    max_height: i64,
) -> (u32, u32) {
    let mut w = width as f64;
    let mut h = height as f64;
    let max_w = max_width as f64;
    let max_h = max_height as f64;

    // "Still the original integer" flags — see the strict-`!==` note in the module
    // docs.  Start true; cleared whenever a dimension is reassigned to a float.
    let mut w_is_raw = true;
    let mut h_is_raw = true;

    if !crop {
        let ratio = max_h / max_w;

        if width == -1 {
            w = h / ratio;
            w_is_raw = false;
        }
        if height == -1 {
            h = w * ratio; // uses the (possibly just-updated) width, as PHP does
            h_is_raw = false;
        }

        let ratio_h = h / max_h;
        let ratio_w = w / max_w;

        match mode {
            // Fill = request is the outer box.
            Mode::Fill => {
                if ratio_h > ratio_w {
                    h = w * ratio;
                    h_is_raw = false;
                } else {
                    w = h / ratio;
                    w_is_raw = false;
                }
            }
            // Cover = request is the inner box.
            Mode::Cover => {
                if ratio_h > ratio_w {
                    w = h / ratio;
                    w_is_raw = false;
                } else {
                    h = w * ratio;
                    h_is_raw = false;
                }
            }
        }
    }

    // Snap each dimension UP to the nearest power of 4 (min 64), scaling the other
    // axis to preserve aspect.  Skipped (PHP strict `!==`) when either dimension is
    // still its original integer equal to the max — the request already IS the max
    // preview, so no bucketing is needed.
    let skip_pow4 = (h_is_raw && h == max_h) || (w_is_raw && w == max_w);
    if !skip_pow4 {
        let pow4_h = pow4_ceil(h).max(64.0);
        let pow4_w = pow4_ceil(w).max(64.0);

        let ratio_h = h / pow4_h;
        let ratio_w = w / pow4_w;

        if ratio_h < ratio_w {
            w = pow4_w;
            h /= ratio_w;
        } else {
            h = pow4_h;
            w /= ratio_h;
        }
    }

    // Clamp into the max preview, preserving aspect (height first, then width — PHP
    // order matters: clamping height changes width before the width clamp runs).
    if h > max_h {
        let ratio = h / max_h;
        h = max_h;
        w /= ratio;
    }
    if w > max_w {
        let ratio = w / max_w;
        w = max_w;
        h /= ratio;
    }

    (round_px(w), round_px(h))
}

/// `4 ** ceil(log4(x))` — snap **up** to the nearest power of 4.
/// PHP: `4 ** ceil(log($x) / log(4))` (natural log).
fn pow4_ceil(x: f64) -> f64 {
    4f64.powf((x.ln() / 4f64.ln()).ceil())
}

/// PHP `(int)round($x)` — round half away from zero, then take the non-negative
/// pixel count.  NaN/±∞ → `0` (PHP `(int)round(NAN)` is `0`).
fn round_px(x: f64) -> u32 {
    if !x.is_finite() {
        return 0;
    }
    x.round().max(0.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: crop=true (mode irrelevant), square max preview.
    fn crop_sq(w: i64, h: i64, max: i64) -> (u32, u32) {
        calculate_size(w, h, true, Mode::Fill, max, max)
    }

    // ── Mode parsing ───────────────────────────────────────────────────────

    #[test]
    fn mode_from_php() {
        assert_eq!(Mode::from_php("fill"), Mode::Fill);
        assert_eq!(Mode::from_php("cover"), Mode::Cover);
        assert_eq!(Mode::from_php(""), Mode::Fill); // PHP default
        assert_eq!(Mode::from_php("bogus"), Mode::Fill);
    }

    // ── Golden vectors from the REAL PHP `Generator::calculateSize` ─────────
    //
    // Generated by invoking the private method via reflection over a corpus of
    // `(w, h, crop, mode, maxW, maxH)` on the Nextcloud reference (the container's
    // `Generator.php`, whose `calculateSize` is byte-identical to the NC33
    // `workspace/server` reference — verified by diff).  This is the authoritative
    // parity check mandated by phase-11 §11.2 ("golden vectors ported from running
    // PHP").  Regenerate with the reflection script in the phase notes if the PHP
    // algorithm ever changes.
    #[test]
    fn golden_vectors_from_php() {
        /// `(w, h, crop, mode, maxW, maxH)` → expected `(expW, expH)`.
        type Case = (i64, i64, bool, Mode, i64, i64, u32, u32);
        #[rustfmt::skip]
        let cases: &[Case] = &[
            // (w, h, crop, mode, maxW, maxH) => (expW, expH)
            // crop=true (mode irrelevant), square max 4096 — power-of-4 snap + min 64 + clamp
            (100, 100, true,  Mode::Fill, 4096, 4096, 256, 256),
            (32,   32,  true,  Mode::Fill, 4096, 4096, 64,  64),
            (100, 200, true,  Mode::Fill, 4096, 4096, 128, 256),
            (10,   10,  true,  Mode::Fill, 4096, 4096, 64,  64),
            (1,    1,   true,  Mode::Fill, 4096, 4096, 64,  64),
            (64,   64,  true,  Mode::Fill, 4096, 4096, 64,  64),
            (65,   65,  true,  Mode::Fill, 4096, 4096, 256, 256),
            (255, 255, true,  Mode::Fill, 4096, 4096, 256, 256),
            (257, 257, true,  Mode::Fill, 4096, 4096, 1024, 1024),
            (1024, 1024, true, Mode::Fill, 4096, 4096, 1024, 1024),
            (256, 256, true,  Mode::Fill, 4096, 4096, 256, 256),
            // equals-max skip + clamp-to-max
            (4096, 4096, true, Mode::Fill, 4096, 4096, 4096, 4096),
            (4096, 100,  true, Mode::Fill, 4096, 4096, 4096, 100),
            (5000, 5000, true, Mode::Fill, 4096, 4096, 4096, 4096),
            (8000, 4000, true, Mode::Fill, 4096, 4096, 4096, 2048),
            // crop=true, non-square max
            (100, 100,  true, Mode::Fill, 2048, 1024, 256, 256),
            (3000, 3000, true, Mode::Fill, 2048, 1024, 1024, 1024),
            // crop=false: fill vs cover, max 4096x2048 (source ratio 0.5)
            (400, 400, false, Mode::Fill,  4096, 2048, 512, 256),
            (400, 400, false, Mode::Cover, 4096, 2048, 1024, 512),
            (333, 333, false, Mode::Fill,  4096, 2048, 512, 256),
            (333, 333, false, Mode::Cover, 4096, 2048, 1024, 512),
            // -1 dimension resolution
            (-1,   512, false, Mode::Fill,  4096, 2048, 1024, 512),
            (1024, -1,  false, Mode::Fill,  4096, 2048, 1024, 512),
            (-1,   256, false, Mode::Cover, 4096, 2048, 512, 256),
            (256,  -1,  false, Mode::Cover, 4096, 2048, 256, 128),
            // crop=false, square max
            (300, 200,  false, Mode::Fill,  4096, 4096, 256, 256),
            (300, 200,  false, Mode::Cover, 4096, 4096, 1024, 1024),
            (1920, 1080, false, Mode::Fill,  4096, 4096, 4096, 4096),
            (1920, 1080, false, Mode::Cover, 4096, 4096, 4096, 4096),
            // crop=false, another non-square max (3000x2000)
            (2000, 1500, false, Mode::Fill,  3000, 2000, 3000, 2000),
            (2000, 1500, false, Mode::Cover, 3000, 2000, 3000, 2000),
        ];
        for &(w, h, crop, mode, max_w, max_h, exp_w, exp_h) in cases {
            let got = calculate_size(w, h, crop, mode, max_w, max_h);
            assert_eq!(
                got,
                (exp_w, exp_h),
                "PHP parity mismatch for ({w},{h},crop={crop},{mode:?},max={max_w}x{max_h})"
            );
        }
    }

    // ── Power-of-4 snap + 64 minimum (crop=true) ───────────────────────────
    //
    // Focused structural cases; the full authoritative parity corpus (generated
    // from the real PHP via reflection) is `golden_vectors_from_php` below.

    #[test]
    fn snap_to_power_of_four() {
        // 100 → 256 (plan's documented example): 4^ceil(log4(100)) = 4^4 = 256.
        assert_eq!(crop_sq(100, 100, 4096), (256, 256));
        // 32 → 64.
        assert_eq!(crop_sq(32, 32, 4096), (64, 64));
        // Non-square: each axis snaps independently, aspect preserved on the
        // dominant axis. 100x200 → 128x256.
        assert_eq!(crop_sq(100, 200, 4096), (128, 256));
    }

    #[test]
    fn minimum_size_is_64() {
        // 10 → pow4 gives 16, clamped up to the 64 minimum.
        assert_eq!(crop_sq(10, 10, 4096), (64, 64));
        assert_eq!(crop_sq(1, 1, 4096), (64, 64));
        // 64 is itself a power of 4 (4^3) and the minimum.
        assert_eq!(crop_sq(64, 64, 4096), (64, 64));
        // 65 snaps up to the next power of 4 (256).
        assert_eq!(crop_sq(65, 65, 4096), (256, 256));
    }

    #[test]
    fn equals_max_skips_bucketing() {
        // Requesting exactly the max preview returns it unchanged (the
        // caller-side "serve the max row" fast path).
        assert_eq!(crop_sq(4096, 4096, 4096), (4096, 4096));
        // One axis at the max also short-circuits the power-of-4 snap (PHP strict
        // `!==`): 4096x100 → width is the integer max → skip → then clamp/scale.
        // With crop=true and w==max, h==100: skip_pow4 (w==max). clamp none. → (4096,100).
        assert_eq!(crop_sq(4096, 100, 4096), (4096, 100));
    }

    #[test]
    fn clamp_to_max_preserving_aspect() {
        // Oversized square request clamps down to the max.
        assert_eq!(crop_sq(5000, 5000, 4096), (4096, 4096));
        // Oversized wide request: snaps up then clamps width to max, height scaled.
        // 8000x4000 → (4096, 2048), aspect 0.5 preserved.
        assert_eq!(crop_sq(8000, 4000, 4096), (4096, 2048));
    }

    // ── Aspect folding (crop=false): fill vs cover ─────────────────────────
    //
    // Max preview 4096x2048 (source ratio h/w = 0.5).

    const MAX_W: i64 = 4096;
    const MAX_H: i64 = 2048;

    #[test]
    fn fill_is_outer_box() {
        // 400x400 fill against a 2:1 source → fits inside → 512x256 (aspect 0.5).
        let (w, h) = calculate_size(400, 400, false, Mode::Fill, MAX_W, MAX_H);
        assert_eq!((w, h), (512, 256));
    }

    #[test]
    fn cover_is_inner_box() {
        // Same request, cover → covers the box → larger: 1024x512 (aspect 0.5).
        let (w, h) = calculate_size(400, 400, false, Mode::Cover, MAX_W, MAX_H);
        assert_eq!((w, h), (1024, 512));
    }

    #[test]
    fn fill_cover_preserve_source_aspect() {
        // Both modes preserve the source aspect ratio (h/w = 0.5).
        for mode in [Mode::Fill, Mode::Cover] {
            let (w, h) = calculate_size(333, 333, false, mode, MAX_W, MAX_H);
            let ratio = h as f64 / w as f64;
            assert!((ratio - 0.5).abs() < 0.02, "{mode:?} aspect drift: {w}x{h}");
        }
    }

    // ── `-1` dimension resolution (crop=false) ─────────────────────────────

    #[test]
    fn resolve_minus_one_width_from_height() {
        // width=-1 derives from height via the source ratio (0.5): h=512 → w=1024,
        // then bucketed → (1024, 512).
        let (w, h) = calculate_size(-1, 512, false, Mode::Fill, MAX_W, MAX_H);
        assert_eq!((w, h), (1024, 512));
    }

    #[test]
    fn resolve_minus_one_height_from_width() {
        // height=-1 derives from width: w=1024 → h=512, then bucketed → (1024, 512).
        let (w, h) = calculate_size(1024, -1, false, Mode::Fill, MAX_W, MAX_H);
        assert_eq!((w, h), (1024, 512));
    }

    // ── Determinism / sanity ───────────────────────────────────────────────

    #[test]
    fn result_is_deterministic_and_positive() {
        for &(w, h, crop, m) in &[
            (256, 256, true, Mode::Fill),
            (300, 200, false, Mode::Fill),
            (300, 200, false, Mode::Cover),
            (1920, 1080, false, Mode::Fill),
            (-1, 256, false, Mode::Cover),
        ] {
            let a = calculate_size(w, h, crop, m, 4096, 4096);
            let b = calculate_size(w, h, crop, m, 4096, 4096);
            assert_eq!(a, b, "non-deterministic for {w}x{h}");
            assert!(a.0 > 0 && a.1 > 0, "non-positive result for {w}x{h}: {a:?}");
            assert!(a.0 <= 4096 && a.1 <= 4096, "exceeds max for {w}x{h}: {a:?}");
        }
    }
}
