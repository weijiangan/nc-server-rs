//! Isolated preview-generation backend — the [`PreviewBackend`] trait and the
//! **Imaginary** HTTP client (Phase 11.4).
//!
//! Generation stays **out-of-process** (no in-process libvips/GD — a malformed image
//! must not be able to crash the server; decoder CVEs stay behind Imaginary's
//! isolation boundary).  The default and only backend today is Imaginary
//! (`Imaginary.php:44-184`); a `vipsthumbnail` subprocess pool can implement the same
//! trait later without touching the orchestration.
//!
//! ## Max-first model (mirrors `Generator::generatePreviews`)
//!
//! - [`PreviewBackend::generate_max`] — the raw source → Imaginary at the clamped max
//!   dims (`preview_max_x/y`, default 4096²) with the **full** pipeline (op1 + `fit`).
//!   The returned dimensions are the **actual** produced size (Imaginary's
//!   `Image-Width`/`Image-Height`), which for a `fit` is the source aspect clamped to
//!   the max box — the value stored in the max row and used to bucket every derived
//!   size.
//! - [`PreviewBackend::render_from_max`] — already-sanitized **max-preview bytes** →
//!   Imaginary with **op2 only** (`fit`/`smartcrop`) at the bucketed size.  PHP derives
//!   in-process (GD) from the decoded max image; Rust re-submits the max bytes instead
//!   — never re-decoding the original per size and never adding an in-process codec.
//!   The returned dimensions are the requested `w`×`h` (PHP stores the bucketed dims
//!   for derived rows, `Generator::generatePreview`).
//!
//! ## Security scoping
//!
//! The `preview_imaginary_url`/`_key` are **sensitive** (`SystemConfig.php:42-43`) and
//! are redacted from [`ImaginaryClient`]'s `Debug` (REQ §17).  Imaginary normally runs
//! on localhost, so this one client talks to a loopback address; that allowance is
//! inherent to this client alone (reqwest imposes no global local-address block to
//! carve out).  The source-size cap ([`ImaginaryClient::check_size`]) is enforced
//! before any POST.

use crate::format::{self, OutputFormat};
use bytes::Bytes;
use std::time::Duration;

/// PHP Imaginary request timeout (`Imaginary.php:153`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// PHP Imaginary connect timeout (`Imaginary.php:154`).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// PHP `preview_max_filesize_image` default, in MiB (`Imaginary.php:49`).
pub const DEFAULT_MAX_FILESIZE_MIB: i64 = 50;

/// Whether the prominent `-return-size` misconfiguration warning has been logged.
/// A misconfigured Imaginary fails **every** max generation the same way, so warn
/// loudly once and then `debug!`, surfacing the issue without flooding the logs on
/// a busy gallery (CLAUDE.md hygiene rule 1: never swallow it — but don't spam).
static WARNED_MISSING_RETURN_SIZE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// A generated preview artifact.
#[derive(Clone, Debug)]
pub struct GeneratedPreview {
    /// The produced image bytes, already in the output format.
    pub bytes: Bytes,
    /// Produced width — Imaginary's `Image-Width` for a max preview; the requested
    /// bucketed width for a derived variant (PHP stores the requested dims there).
    pub width: u32,
    /// Produced height (see [`Self::width`]).
    pub height: u32,
    /// The produced output mimetype (`image/jpeg`/`image/png`/`image/webp`).
    pub output_mime: String,
}

/// A generation failure.  Every variant means "fall back to PHP-FPM" — the caller
/// proxies the original request (PHP may still generate via GD/imagick/other
/// providers), so behaviour is never worse than today.
#[derive(thiserror::Error, Debug)]
pub enum BackendError {
    /// Source exceeds `preview_max_filesize_image` — rejected before any HTTP call
    /// (PHP returns `null` → `NotFoundException` → 404).
    #[error("source of {size} bytes exceeds the {cap}-byte cap (preview_max_filesize_image)")]
    TooLarge { size: u64, cap: u64 },
    /// Network/transport failure (connect timeout, DNS, reset, body read, …).
    #[error("imaginary request failed: {0}")]
    Transport(String),
    /// Imaginary returned a non-200 (it reports a JSON `message`; PHP logs it and
    /// returns `null`).
    #[error("imaginary returned HTTP {status}: {message}")]
    Status { status: u16, message: String },
    /// The max-preview response omitted `Image-Width`/`Image-Height`.  Rust has no
    /// in-process decoder to recover the dimensions, so Imaginary must be started with
    /// `-return-size` (the standard Nextcloud setup); otherwise generation degrades to
    /// the PHP-FPM fallback (which decodes).
    #[error("imaginary response omitted Image-Width/Image-Height (Imaginary needs -return-size)")]
    MissingDimensions,
    /// Imaginary returned an empty body.
    #[error("imaginary returned an empty image body")]
    Empty,
}

/// A pluggable, isolated preview generator.  Object-safe so the orchestration can
/// hold an `Arc<dyn PreviewBackend>` and swap backends without touching call sites.
#[async_trait::async_trait]
pub trait PreviewBackend: Send + Sync {
    /// Generate the **max** preview from the raw source bytes (full pipeline).
    async fn generate_max(
        &self,
        source: Bytes,
        source_mime: &str,
        max_w: u32,
        max_h: u32,
    ) -> Result<GeneratedPreview, BackendError>;

    /// Derive a smaller variant from **already-sanitized max-preview bytes** (op2
    /// only).  `source_mime` is the *original* source mimetype — it resolves the output
    /// format (which the max bytes are already in); the POST `Content-Type` is that
    /// output format, not the original source mime.
    async fn render_from_max(
        &self,
        max_bytes: Bytes,
        source_mime: &str,
        w: u32,
        h: u32,
        crop: bool,
    ) -> Result<GeneratedPreview, BackendError>;
}

/// The Imaginary HTTP backend (`Imaginary.php`).
pub struct ImaginaryClient {
    http: reqwest::Client,
    /// `preview_imaginary_url` with any trailing `/` stripped (PHP `rtrim(…, '/')`).
    url: String,
    /// `preview_imaginary_key` — sent as a **query parameter** (may be empty).
    key: String,
    /// `preview_format` (only `webp` is honoured).
    preview_format: Option<String>,
    /// Resolved `preview/jpeg_quality` appconfig (default `"80"`).
    jpeg_quality: String,
    /// Resolved `preview/webp_quality` appconfig (default `"80"`).
    webp_quality: String,
    /// `preview_max_filesize_image` in MiB; `-1` disables the cap.
    max_filesize_mib: i64,
}

/// REQ §17: the URL and key are sensitive — never leak them via `Debug`.
impl std::fmt::Debug for ImaginaryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImaginaryClient")
            .field("url", &"<redacted>")
            .field("key", &"<redacted>")
            .field("preview_format", &self.preview_format)
            .field("jpeg_quality", &self.jpeg_quality)
            .field("webp_quality", &self.webp_quality)
            .field("max_filesize_mib", &self.max_filesize_mib)
            .finish()
    }
}

impl ImaginaryClient {
    /// Build a client, or `None` when Imaginary is **not configured** — PHP treats the
    /// default `'invalid'` and the empty string as unset (`Imaginary.php:57-61`, logs
    /// an error and returns `null`).  `jpeg_quality`/`webp_quality` are the resolved
    /// appconfig values (default `"80"`); `max_filesize_mib` is
    /// `preview_max_filesize_image` (default `50`, `-1` unlimited).
    pub fn new(
        url: &str,
        key: &str,
        preview_format: Option<String>,
        jpeg_quality: String,
        webp_quality: String,
        max_filesize_mib: i64,
    ) -> Option<Self> {
        if url.is_empty() || url == "invalid" {
            return None;
        }
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .ok()?;
        Some(Self {
            http,
            url: url.trim_end_matches('/').to_string(),
            key: key.to_string(),
            preview_format,
            jpeg_quality,
            webp_quality,
            max_filesize_mib,
        })
    }

    /// Whether this client can attempt the given source mimetype.  Pure gating lives
    /// in `ProviderRegistry::rust_generatable` (11.1); this is the per-request size
    /// gate (PHP checks it inside the provider, `Imaginary.php:49-55`).
    pub fn supports_size(&self, source_len: usize) -> bool {
        self.check_size(source_len).is_ok()
    }

    /// PHP `Imaginary.php:49-55`: reject an oversized source **before** any HTTP call.
    fn check_size(&self, len: usize) -> Result<(), BackendError> {
        if self.max_filesize_mib != -1 {
            let cap = (self.max_filesize_mib as u64).saturating_mul(1024 * 1024);
            if (len as u64) > cap {
                return Err(BackendError::TooLarge {
                    size: len as u64,
                    cap,
                });
            }
        }
        Ok(())
    }

    /// Build the `/pipeline` request without sending it (PHP `Imaginary.php:146-155`):
    /// `operations` + `key` in the **query string** (not headers), the body's mimetype
    /// as `Content-Type`, and the raw bytes as the body.  Exposed for the request-shape
    /// test; [`Self::run`] sends it.
    fn build_request(
        &self,
        body: Bytes,
        content_type: &str,
        operations_json: &str,
    ) -> Result<reqwest::Request, BackendError> {
        self.http
            .post(format!("{}/pipeline", self.url))
            .query(&[("operations", operations_json), ("key", self.key.as_str())])
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .build()
            .map_err(|e| BackendError::Transport(e.to_string()))
    }

    /// POST a pipeline and parse the response.  `requested_dims` is `Some((w,h))` for a
    /// derived variant (PHP records the requested bucketed dims, so those are used
    /// verbatim) and `None` for the max preview (dims come from Imaginary's
    /// `Image-Width`/`Image-Height`; their absence is [`BackendError::MissingDimensions`]).
    async fn run(
        &self,
        body: Bytes,
        content_type: &str,
        operations_json: &str,
        out: OutputFormat,
        requested_dims: Option<(u32, u32)>,
    ) -> Result<GeneratedPreview, BackendError> {
        let req = self.build_request(body, content_type, operations_json)?;
        let resp = self
            .http
            .execute(req)
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            let message = resp.text().await.unwrap_or_default();
            return Err(BackendError::Status {
                status: status.as_u16(),
                message,
            });
        }
        let img_w = header_u32(resp.headers(), "Image-Width");
        let img_h = header_u32(resp.headers(), "Image-Height");
        let response_mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if bytes.is_empty() {
            return Err(BackendError::Empty);
        }
        let (width, height) = match requested_dims {
            Some(dims) => dims,
            None => match (img_w, img_h) {
                (Some(w), Some(h)) => (w, h),
                _ => {
                    use std::sync::atomic::Ordering;
                    // Imaginary didn't report the produced dimensions.  PHP recovers by
                    // decoding the bytes; Rust has no in-process decoder, so this is an
                    // Imaginary deployment misconfiguration — surface it and fall back
                    // to PHP-FPM (which decodes).
                    if !WARNED_MISSING_RETURN_SIZE.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            "Imaginary response omitted Image-Width/Image-Height: start \
                             Imaginary with `-return-size` to enable native max-preview \
                             generation. Falling back to PHP-FPM. (Logged once; further \
                             occurrences are debug-level.)"
                        );
                    } else {
                        tracing::debug!(
                            "Imaginary response still omits Image-Width/Image-Height \
                             (`-return-size` not set); falling back to PHP-FPM"
                        );
                    }
                    return Err(BackendError::MissingDimensions);
                }
            },
        };
        Ok(GeneratedPreview {
            bytes,
            width,
            height,
            output_mime: response_mime.unwrap_or_else(|| out.mime().to_string()),
        })
    }
}

#[async_trait::async_trait]
impl PreviewBackend for ImaginaryClient {
    async fn generate_max(
        &self,
        source: Bytes,
        source_mime: &str,
        max_w: u32,
        max_h: u32,
    ) -> Result<GeneratedPreview, BackendError> {
        self.check_size(source.len())?;
        let out = format::output_format(source_mime, self.preview_format.as_deref());
        let ops = format::max_operations_json(
            source_mime,
            max_w,
            max_h,
            self.preview_format.as_deref(),
            &self.jpeg_quality,
            &self.webp_quality,
        );
        // Content-Type = the SOURCE mimetype (PHP `'content-type' => $file->getMimeType()`).
        self.run(source, source_mime, &ops, out, None).await
    }

    async fn render_from_max(
        &self,
        max_bytes: Bytes,
        source_mime: &str,
        w: u32,
        h: u32,
        crop: bool,
    ) -> Result<GeneratedPreview, BackendError> {
        let out = format::output_format(source_mime, self.preview_format.as_deref());
        let quality = format::quality_for(out, &self.jpeg_quality, &self.webp_quality);
        let ops = format::derive_operations_json(w, h, crop, out, &quality);
        // Content-Type = the max preview's output format (the bytes being submitted),
        // NOT the original source mime — the max bytes are already converted.
        self.run(max_bytes, out.mime(), &ops, out, Some((w, h)))
            .await
    }
}

/// Parse a response header as a trimmed `u32` (Imaginary's `Image-Width`/`Image-Height`).
fn header_u32(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u32> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(url: &str) -> ImaginaryClient {
        ImaginaryClient::new(url, "sekrit-key", None, "80".into(), "80".into(), 50).unwrap()
    }

    // ── construction / config gating ───────────────────────────────────────

    #[test]
    fn new_rejects_unconfigured_url() {
        // PHP default 'invalid' and '' mean "not configured" → no client.
        assert!(ImaginaryClient::new("", "", None, "80".into(), "80".into(), 50).is_none());
        assert!(ImaginaryClient::new("invalid", "", None, "80".into(), "80".into(), 50).is_none());
        assert!(ImaginaryClient::new(
            "http://localhost:9090",
            "",
            None,
            "80".into(),
            "80".into(),
            50
        )
        .is_some());
    }

    #[test]
    fn timeouts_match_php() {
        // Imaginary.php:153-154 — 120 s request, 3 s connect.
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(120));
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(3));
    }

    #[test]
    fn debug_redacts_url_and_key() {
        let c = client("http://imaginary.internal:9090");
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("imaginary.internal"), "URL leaked: {dbg}");
        assert!(!dbg.contains("sekrit-key"), "key leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));
    }

    // ── request shape (build, no send) ─────────────────────────────────────

    #[test]
    fn request_shape_matches_php() {
        // A loopback URL must be accepted (Imaginary normally runs on localhost).
        let c = client("http://127.0.0.1:9090/"); // trailing slash stripped
        let ops = format::max_operations_json("image/jpeg", 4096, 4096, None, "80", "80");
        let req = c
            .build_request(Bytes::from_static(b"SRC"), "image/jpeg", &ops)
            .unwrap();

        // Path + the operations/key live in the QUERY STRING, not headers.
        assert_eq!(req.url().path(), "/pipeline");
        let pairs: std::collections::HashMap<String, String> = req
            .url()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            pairs.get("operations").map(String::as_str),
            Some(ops.as_str())
        );
        assert_eq!(pairs.get("key").map(String::as_str), Some("sekrit-key"));

        // Content-Type = the source mimetype; operations/key are NOT headers.
        assert_eq!(
            req.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
            "image/jpeg"
        );
        assert!(req.headers().get("operations").is_none());
        assert!(req.headers().get("key").is_none());

        // Body is the raw source.
        assert_eq!(req.body().and_then(|b| b.as_bytes()).unwrap(), b"SRC");
    }

    #[test]
    fn derived_request_uses_max_output_mime_as_content_type() {
        let c = client("http://127.0.0.1:9090");
        // A HEIC source's max preview is jpeg; re-submitting it must declare image/jpeg.
        let out = format::output_format("image/heic", None);
        assert_eq!(out, OutputFormat::Jpeg);
        let ops = format::derive_operations_json(256, 256, true, out, "80");
        let req = c
            .build_request(Bytes::from_static(b"MAX"), out.mime(), &ops)
            .unwrap();
        assert_eq!(
            req.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
            "image/jpeg"
        );
    }

    // ── filesize cap (no HTTP call) ────────────────────────────────────────

    #[test]
    fn filesize_cap_rejects_before_post() {
        let c = client("http://127.0.0.1:1"); // unreachable — proves no POST is attempted
        let mib = 1024 * 1024u64;
        // Over the 50 MiB cap → TooLarge (not a transport error, so no POST happened).
        let big = Bytes::from(vec![0u8; (50 * mib as usize) + 1]);
        match futures::executor::block_on(c.generate_max(big, "image/jpeg", 4096, 4096)) {
            Err(BackendError::TooLarge { cap, .. }) => assert_eq!(cap, 50 * mib),
            other => panic!("expected TooLarge, got {other:?}"),
        }
        // At/under the cap passes the size check (would proceed to POST).
        assert!(c.check_size(50 * mib as usize).is_ok());
        assert!(c.check_size(1024).is_ok());
    }

    #[test]
    fn filesize_cap_disabled_by_minus_one() {
        let c = ImaginaryClient::new(
            "http://127.0.0.1:9090",
            "",
            None,
            "80".into(),
            "80".into(),
            -1,
        )
        .unwrap();
        // -1 = unlimited: a huge source passes the size gate.
        assert!(c.check_size(usize::MAX / 2).is_ok());
        assert!(c.supports_size(usize::MAX / 2));
    }

    // ── round-trip against a mock Imaginary server ─────────────────────────

    #[derive(Default)]
    struct Captured {
        operations: Option<String>,
        key: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    /// A canned image response: `Image-Width: 100`, `Image-Height: 80`,
    /// `Content-Type: image/jpeg`, body = a few JPEG marker bytes.
    const FAKE_IMAGE: &[u8] = &[0xFF, 0xD8, 0xFF, 0xD9];

    async fn spawn_mock(
        status: axum::http::StatusCode,
    ) -> (String, std::sync::Arc<tokio::sync::Mutex<Captured>>) {
        use axum::{extract::State, routing::post, Router};
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Captured::default()));
        let cap = captured.clone();
        let app = Router::new()
            .route(
                "/pipeline",
                post(
                    move |State(cap): State<std::sync::Arc<tokio::sync::Mutex<Captured>>>,
                          axum::extract::Query(q): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >,
                          headers: axum::http::HeaderMap,
                          body: axum::body::Bytes| async move {
                        let mut c = cap.lock().await;
                        c.operations = q.get("operations").cloned();
                        c.key = q.get("key").cloned();
                        c.content_type = headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from);
                        c.body = body.to_vec();
                        drop(c);
                        let mut h = axum::http::HeaderMap::new();
                        h.insert("Image-Width", "100".parse().unwrap());
                        h.insert("Image-Height", "80".parse().unwrap());
                        h.insert(
                            axum::http::header::CONTENT_TYPE,
                            "image/jpeg".parse().unwrap(),
                        );
                        (status, h, axum::body::Body::from(FAKE_IMAGE.to_vec()))
                    },
                ),
            )
            .with_state(cap);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), captured)
    }

    #[tokio::test]
    async fn generate_max_roundtrip_parses_response() {
        let (url, cap) = spawn_mock(axum::http::StatusCode::OK).await;
        let c = ImaginaryClient::new(&url, "k", None, "80".into(), "80".into(), 50).unwrap();
        let src = Bytes::from_static(b"RAWSOURCE");
        let gp = c
            .generate_max(src.clone(), "image/png", 4096, 4096)
            .await
            .unwrap();

        // Parsed from the response headers/body.
        assert_eq!(gp.bytes.as_ref(), FAKE_IMAGE);
        assert_eq!((gp.width, gp.height), (100, 80));
        assert_eq!(gp.output_mime, "image/jpeg");

        // The server received the PNG max pipeline + source content-type + body.
        let got = cap.lock().await;
        assert_eq!(
            got.operations.as_deref(),
            Some(format::max_operations_json("image/png", 4096, 4096, None, "80", "80").as_str())
        );
        assert_eq!(got.content_type.as_deref(), Some("image/png"));
        assert_eq!(got.key.as_deref(), Some("k"));
        assert_eq!(got.body, b"RAWSOURCE");
    }

    #[tokio::test]
    async fn render_from_max_uses_requested_dims_and_max_mime() {
        let (url, cap) = spawn_mock(axum::http::StatusCode::OK).await;
        let c = ImaginaryClient::new(&url, "k", None, "80".into(), "80".into(), 50).unwrap();
        // HEIC source → max is jpeg; derive a cropped 256×256 from the max bytes.
        let gp = c
            .render_from_max(Bytes::from_static(b"MAXJPEG"), "image/heic", 256, 256, true)
            .await
            .unwrap();

        // Dims are the REQUESTED 256×256, not the mock's Image-Width/Height (100×80).
        assert_eq!((gp.width, gp.height), (256, 256));
        assert_eq!(gp.output_mime, "image/jpeg");

        // The server got the op2-only smartcrop pipeline and the max's output mime.
        let got = cap.lock().await;
        assert_eq!(
            got.operations.as_deref(),
            Some(format::derive_operations_json(256, 256, true, OutputFormat::Jpeg, "80").as_str())
        );
        assert_eq!(got.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(got.body, b"MAXJPEG");
    }

    #[tokio::test]
    async fn non_200_is_a_status_error() {
        let (url, _cap) = spawn_mock(axum::http::StatusCode::BAD_REQUEST).await;
        let c = ImaginaryClient::new(&url, "k", None, "80".into(), "80".into(), 50).unwrap();
        let err = c
            .generate_max(Bytes::from_static(b"X"), "image/jpeg", 4096, 4096)
            .await
            .unwrap_err();
        match err {
            BackendError::Status { status, .. } => assert_eq!(status, 400),
            other => panic!("expected Status, got {other:?}"),
        }
    }
}
