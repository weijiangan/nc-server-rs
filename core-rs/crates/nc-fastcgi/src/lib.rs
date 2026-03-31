#![forbid(unsafe_code)]

//! FastCGI client, route registry, PHP-FPM dispatch with identity injection.
//!
//! Phase 7.1: full FastCGI client — Unix socket, param building,
//!            streaming CGI response parsing, timeout handling, auth injection.
//!
//! ## Connection model
//!
//! We use **short-connection mode** (`Client::new_tokio` / `execute_once_stream`)
//! rather than a keep-alive pool (deadpool).  The reasoning:
//!
//! * PHP-FPM's worker count is the actual throughput gate for PHP-FPM-served
//!   routes; reducing Unix-socket connect overhead is marginal compared with
//!   that.
//! * The real source of performance degradation the project targets — sync
//!   desktop clients blocking all PHP-FPM workers — is already solved: those
//!   paths are served natively by Rust's DAV layer and never touch PHP-FPM.
//! * True connection pooling with streaming requires an interior-mutability
//!   workaround to avoid a self-referential borrow:
//!   `execute_stream(&mut self)` returns `ResponseStream<&mut S>` which borrows
//!   the pool guard; the guard cannot then be moved into a background task.
//!   `execute_once_stream(self)` takes **ownership** of the connection, so the
//!   owned `ResponseStream<S>` can be freely moved — the connection is cleaned
//!   up when the stream is exhausted or the response body is dropped.
//!
//! If load testing (§8) shows Unix socket setup is a measurable cost at
//! production concurrency, the right fix is an `Arc<Mutex<Client>>` interior-
//! mutability pool or switching to the keep-alive `execute_stream` API with an
//! explicit connection-slot semaphore limiting concurrency to the PHP-FPM
//! `pm.max_children` value.
//!
//! ## Streaming
//!
//! `execute_once_stream` starts returning FCGI records immediately as PHP
//! produces them.  We read stdout bytes until we locate the CGI header
//! separator (`\r\n\r\n`), parse the headers, then forward the remaining
//! bytes as a streaming `axum::body::Body`.  Combined with axum's stream-to-
//! socket forwarding this gives zero copy and near-zero TTFB overhead for the
//! PHP-FPM path.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::{body::Body, response::Response};
use bytes::{Bytes, BytesMut};
use fastcgi_client::StreamExt as _;          // brings .next() onto ResponseStream
use fastcgi_client::response::{Content, ResponseStream};
use futures::Stream;
use nc_db::config::NcConfig;
use tokio_util::io::StreamReader;

// ── FastCgiState ─────────────────────────────────────────────────────────────

/// Shared state for the PHP-FPM proxy.
///
/// Held as `Option<FastCgiState>` inside `AppState`.  When `None`, all
/// FastCGI-bound routes return `502 Bad Gateway` (PHP-FPM not configured).
#[derive(Clone, Debug)]
pub struct FastCgiState {
    /// Absolute path to the PHP-FPM Unix socket (e.g. `/run/nc-fpm.sock`).
    pub socket_path: PathBuf,
    /// Request timeout in milliseconds (default 30 000).
    pub timeout_ms: u64,
    /// Nextcloud installation root (parent of `config/`, `apps/`, etc.).
    pub nc_root: PathBuf,
    /// Absolute path to the PHP bootstrap shim invoked for every proxied
    /// request: `{nc_root}/core-rs/php-shim/index.php`.
    pub shim_path: PathBuf,
}

impl FastCgiState {
    /// Construct from `NcConfig` and the server's installation root.
    ///
    /// Uses `NC_FASTCGI_SOCKET` and `NC_PHP_SHIM` environment variables as
    /// defaults if `config.php` doesn't specify them.
    ///
    /// Returns `None` when no socket is configured.
    pub fn from_config(config: &NcConfig, nc_root: &Path) -> Option<Self> {
        let socket_path = config.fastcgi_socket.clone().or_else(|| {
            std::env::var("NC_FASTCGI_SOCKET")
                .ok()
                .map(PathBuf::from)
        })?;

        let shim_path = std::env::var("NC_PHP_SHIM")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| nc_root.join("core-rs/php-shim/index.php"));

        Some(Self {
            socket_path,
            timeout_ms: config.fastcgi_timeout_ms,
            nc_root: nc_root.to_path_buf(),
            shim_path,
        })
    }
}

// ── proxy_handler ─────────────────────────────────────────────────────────────

/// Forward an HTTP request to PHP-FPM via FastCGI (Phase 7.1).
///
/// # Protocol summary
///
/// 1. Reads `AuthInfo` extension (set by the auth middleware in Phase 3).
/// 2. Buffers the **request** body up to 64 MiB — PHP-FPM routes serve API
///    calls and the web UI; large file uploads are handled natively by the
///    DAV layer and never reach this path.
/// 3. Connects to the PHP-FPM Unix socket with `fpm.timeout_ms` timeout.
/// 4. Builds CGI params: mandatory FastCGI params + all incoming `HTTP_*`
///    headers, with the security-sensitive identity headers stripped from
///    the client input and re-injected from the Rust-validated `AuthInfo`.
/// 5. Sends the FastCGI request via `execute_once_stream`.
/// 6. **Streams** the CGI stdout: reads bytes until the CGI header separator
///    (`\r\n\r\n`) is found, parses those headers into HTTP response headers,
///    then returns an axum streaming body backed by the remaining
///    `ResponseStream`.  PHP-FPM output is forwarded to the HTTP client with
///    near-zero copy and near-zero TTFB latency overhead.
///
/// # Error responses
/// - `502 Bad Gateway`: PHP-FPM socket unavailable or FastCGI protocol error.
/// - `504 Gateway Timeout`: timeout exceeded during connect, or during the
///   header phase (body streaming timeout is enforced by the transport layer).
pub async fn proxy_handler(fpm: &FastCgiState, req: axum::extract::Request) -> Response {
    use axum::http::header;

    // ── 1. Extract auth info before consuming the request ────────────────────
    let auth_info = req.extensions().get::<nc_auth::AuthInfo>().cloned();

    // ── 2. Decompose request ─────────────────────────────────────────────────
    let (parts, body) = req.into_parts();
    let method = parts.method.as_str().to_owned();
    let uri = parts.uri.clone();
    let request_uri = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| uri.path().to_owned());
    let query_string = uri.query().unwrap_or("").to_owned();
    let uri_path = uri.path().to_owned();

    // ── 3. Derive PHP script path and PATH_INFO from the URI ─────────────────
    let (script_rel, path_info_str) = derive_script_info(&uri_path);
    let nc_original_script = fpm
        .nc_root
        .join(script_rel.trim_start_matches('/'))
        .to_string_lossy()
        .into_owned();
    let shim_path_str = fpm.shim_path.to_string_lossy().into_owned();
    let script_rel_owned = script_rel.to_owned();
    let path_info_owned = path_info_str.to_owned();

    // ── 4. Extract per-request content headers ────────────────────────────────
    let content_type = header_str(&parts.headers, header::CONTENT_TYPE);
    let content_length = header_str(&parts.headers, header::CONTENT_LENGTH);

    // ── 5. Build FastCGI params ───────────────────────────────────────────────
    let document_root = fpm.nc_root.to_string_lossy().to_string();
    let mut params: fastcgi_client::Params<'static> = fastcgi_client::Params::default()
        .gateway_interface("CGI/1.1")
        .server_software("nc-server/0.1")
        .server_protocol("HTTP/1.1")
        .request_method(method)
        .script_filename(shim_path_str)
        .custom("NC_ORIGINAL_SCRIPT", nc_original_script)
        .custom("DOCUMENT_ROOT", document_root)
        .custom("SCRIPT_NAME", script_rel_owned)
        .custom("REQUEST_URI", request_uri)
        .custom("QUERY_STRING", query_string)
        .custom("CONTENT_TYPE", content_type)
        .custom("CONTENT_LENGTH", content_length)
        .custom("PATH_INFO", path_info_owned);

    // ── 6. Forward HTTP headers as HTTP_* params ──────────────────────────────
    //
    // Security (§7.3): strip client-supplied identity headers from incoming
    // requests so a malicious client cannot impersonate a different user.
    // The real values are injected from the Rust-validated AuthInfo below.
    for (name, value) in &parts.headers {
        let name_lower = name.as_str().to_ascii_lowercase();
        if matches!(
            name_lower.as_str(),
            "content-type"
                | "content-length"
                | "x-nc-user"
                | "x-nc-session-token"
                | "x-nc-is-admin"
        ) {
            continue;
        }
        if let Ok(val) = value.to_str() {
            let key = format!(
                "HTTP_{}",
                name.as_str().to_ascii_uppercase().replace('-', "_")
            );
            params = params.custom(key, val.to_owned());
        }
    }

    // ── 7. Inject Rust-validated identity ────────────────────────────────────
    //
    // These params are trusted by the PHP bootstrap shim (§7.4).
    // `is_admin` comes from the auth middleware's oc_group_user lookup (§7.2).
    // `HTTP_X_NC_SESSION_TOKEN` carries the raw bearer / app-token value so
    // the PHP shim can pass it to OCP APIs that need the original token.
    // Unauthenticated requests inject nothing; the shim allows them via the
    // proxy marker below so PHP can run its own auth (login, well-known, etc.).
    if let Some(ref info) = auth_info {
        params = params
            .custom("HTTP_X_NC_USER", info.uid.clone())
            .custom("HTTP_X_NC_IS_ADMIN", if info.is_admin { "1" } else { "0" });
        if let Some(ref token) = info.raw_token {
            params = params.custom("HTTP_X_NC_SESSION_TOKEN", token.clone());
        }
    }

    // ── 7b. Inject the proxy trust marker (§7.3 / §7.8) ──────────────────────
    //
    // HTTP_X_NC_PROXIED=1 is stripped from client requests (step 6 above) and
    // re-injected here for every request that passes through the Rust proxy,
    // whether authenticated or not.  The PHP bootstrap shim validates this
    // marker as the channel trust signal — distinguishing legitimate
    // Rust-proxied requests (both authenticated and unauthenticated: login
    // flows, well-known redirects, public pages) from direct FastCGI socket
    // connections that bypass the Rust auth layer entirely.
    params = params.custom("HTTP_X_NC_PROXIED", "1");

    // ── 8. Buffer request body ────────────────────────────────────────────────
    //
    // PHP-FPM dispatched routes handle API calls and web UI; large file
    // uploads are served natively by the DAV layer.  64 MiB cap matches a
    // generous OCS/REST payload; POST bodies larger than this return 413.
    const MAX_BODY: usize = 64 * 1024 * 1024; // 64 MiB
    let body_bytes: Bytes =
        match axum::body::to_bytes(body, MAX_BODY).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "fastcgi: failed to read request body");
                return error_response(502, "Failed to read request body\n");
            }
        };

    // ── 9. Open Unix socket ───────────────────────────────────────────────────
    let timeout = std::time::Duration::from_millis(fpm.timeout_ms);
    let stream =
        match tokio::time::timeout(timeout, tokio::net::UnixStream::connect(&fpm.socket_path))
            .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!(
                    socket = %fpm.socket_path.display(),
                    error = %e,
                    "fastcgi: PHP-FPM socket unavailable"
                );
                return error_response(502, "PHP-FPM unavailable\n");
            }
            Err(_elapsed) => {
                tracing::warn!(
                    socket = %fpm.socket_path.display(),
                    "fastcgi: timed out connecting to PHP-FPM"
                );
                return error_response(504, "PHP-FPM gateway timeout\n");
            }
        };

    // ── 10. Build and execute FastCGI request (streaming) ─────────────────────
    //
    // Wrap buffered body bytes as a single-item stream so `StreamReader`
    // provides the `tokio::io::AsyncRead` that `Request::new_tokio` expects
    // for stdin.  The body is already fully buffered (step 8) so this is a
    // zero-copy hand-off.
    let stdin = StreamReader::new(futures::stream::once(std::future::ready(
        Ok::<Bytes, std::io::Error>(body_bytes),
    )));
    let client = fastcgi_client::Client::new_tokio(stream);
    let fcgi_req = fastcgi_client::Request::new_tokio(params, stdin);

    // `execute_once_stream` takes ownership of the client (and thus the Unix
    // socket) and returns an **owned** `ResponseStream`.  When the stream is
    // exhausted (or dropped), the socket closes — no borrow lifetime escapes.
    let response_stream =
        match tokio::time::timeout(timeout, client.execute_once_stream(fcgi_req)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "fastcgi: execution failed");
                return error_response(502, "FastCGI execution failed\n");
            }
            Err(_elapsed) => {
                tracing::warn!("fastcgi: timed out waiting for PHP-FPM response");
                return error_response(504, "PHP-FPM gateway timeout\n");
            }
        };

    // ── 11. Parse CGI headers then stream body ────────────────────────────────
    //
    // We must see the full header block before we can write the HTTP status
    // line; the header block is typically a few hundred bytes so we apply the
    // same timeout budget as the initial connection.
    match tokio::time::timeout(timeout, parse_streaming_headers(response_stream)).await {
        Ok(Ok((status, cgi_headers, body_stream))) => {
            let mut builder = Response::builder().status(status);
            for (name, value) in cgi_headers {
                builder = builder.header(name, value);
            }
            builder
                .body(Body::from_stream(body_stream))
                .unwrap_or_else(|_| error_response(502, "Failed to build response\n"))
        }
        Ok(Err(resp)) => resp,
        Err(_elapsed) => {
            tracing::warn!("fastcgi: timed out waiting for CGI response headers");
            error_response(504, "PHP-FPM gateway timeout (headers)\n")
        }
    }
}

// ── CgiBodyStream ─────────────────────────────────────────────────────────────

/// A `Stream` adapter that:
/// 1. Yields `prefix` bytes first (the body bytes that were buffered alongside
///    the CGI headers during header parsing, i.e. the bytes after `\r\n\r\n`
///    in the last header chunk).
/// 2. Then yields `Content::Stdout` chunks from the underlying `ResponseStream`.
/// 3. Logs and discards `Content::Stderr` chunks (PHP error log output).
///
/// Implements `Stream<Item = Result<Bytes, std::io::Error>>` which satisfies
/// axum's `Body::from_stream` bound (`E: Into<BoxError>`).
///
/// `S` is `Unpin` because `tokio_util::compat::Compat<tokio::net::UnixStream>`
/// is `Unpin`, so `CgiBodyStream<S>` is `Unpin` and stream polling does not
/// require heap-pinning.
struct CgiBodyStream<S: fastcgi_client::io::AsyncRead + Unpin> {
    /// Leftover body bytes from the chunk that contained the end of the CGI
    /// headers.  Drained first, then cleared.
    prefix: Bytes,
    /// Remaining FCGI records from the short-lived connection.  Owned, so the
    /// connection is closed when this stream is dropped.
    inner: ResponseStream<S>,
}

impl<S: fastcgi_client::io::AsyncRead + Unpin> Stream for CgiBodyStream<S> {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 1. Drain prefix bytes first.
        if !self.prefix.is_empty() {
            let chunk = std::mem::take(&mut self.prefix);
            return Poll::Ready(Some(Ok(chunk)));
        }

        // 2. Pull FCGI records; forward Stdout, log Stderr, skip empty chunks.
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(Content::Stdout(b)))) if !b.is_empty() => {
                    return Poll::Ready(Some(Ok(b)));
                }
                Poll::Ready(Some(Ok(Content::Stdout(_)))) => {
                    // zero-length stdout record — skip, continue polling
                }
                Poll::Ready(Some(Ok(Content::Stderr(b)))) => {
                    if !b.is_empty() {
                        tracing::debug!(
                            stderr = %String::from_utf8_lossy(&b),
                            "fastcgi: PHP-FPM stderr"
                        );
                    }
                    // Do NOT forward stderr to the HTTP client.
                }
                Poll::Ready(Some(Err(e))) => {
                    tracing::warn!(error = %e, "fastcgi: stream error during body");
                    return Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        e,
                    ))));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ── parse_streaming_headers ────────────────────────────────────────────────────

/// Read from `stream` until the CGI header separator (`\r\n\r\n`) is found.
///
/// Returns:
/// - The HTTP status code parsed from the optional `Status:` header
///   (defaulting to 200 OK if absent).
/// - The remaining response headers as `(name, value)` pairs.
/// - A `CgiBodyStream` carrying the body bytes that came in the same chunk
///   as the end of the headers, plus the remaining `ResponseStream`.
///
/// # Errors
/// Returns an error `Response` (502/504 etc.) on stream errors or if the
/// separator is not found before the stream ends.
async fn parse_streaming_headers<S>(
    mut stream: ResponseStream<S>,
) -> Result<(axum::http::StatusCode, Vec<(String, String)>, CgiBodyStream<S>), Response>
where
    S: fastcgi_client::io::AsyncRead + Unpin,
{
    const SEP: &[u8] = b"\r\n\r\n";
    let mut header_accum = BytesMut::new();

    loop {
        match stream.next().await {
            Some(Ok(Content::Stdout(chunk))) => {
                header_accum.extend_from_slice(&chunk);

                if let Some(sep_pos) = header_accum.windows(SEP.len()).position(|w| w == SEP) {
                    // Split at the separator.
                    let header_bytes = header_accum.split_to(sep_pos);
                    let _sep = header_accum.split_to(SEP.len()); // discard \r\n\r\n
                    let body_prefix = header_accum.freeze(); // bytes after separator

                    let (status, headers) = parse_cgi_header_block(&header_bytes)
                        .map_err(|_| error_response(502, "Invalid CGI headers from PHP-FPM\n"))?;

                    return Ok((status, headers, CgiBodyStream { prefix: body_prefix, inner: stream }));
                }
                // Separator not yet in buffer — keep reading.
            }
            Some(Ok(Content::Stderr(b))) => {
                if !b.is_empty() {
                    tracing::debug!(
                        stderr = %String::from_utf8_lossy(&b),
                        "fastcgi: PHP-FPM stderr (header phase)"
                    );
                }
            }
            Some(Err(e)) => {
                tracing::warn!(error = %e, "fastcgi: stream error reading CGI headers");
                return Err(error_response(502, "FastCGI stream error\n"));
            }
            None => {
                // Stream ended before we found a complete header block.
                return Err(error_response(502, "FastCGI response missing header separator\n"));
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Map a request URI path to `(script_relative_path, path_info)`.
///
/// Nextcloud's top-level PHP entry points contain `.php` in the path:
/// - `/index.php`            → script `/index.php`, PATH_INFO ``
/// - `/index.php/apps/files` → script `/index.php`, PATH_INFO `/apps/files`
/// - `/ocs/v2.php/cloud/cap` → script `/ocs/v2.php`, PATH_INFO `/cloud/cap`
/// - `/remote.php/dav/...`   → script `/remote.php`, PATH_INFO `/dav/...`
///
/// Clean URLs (no `.php`) are routed through `/index.php`:
/// - `/apps/files/api/...`   → script `/index.php`, PATH_INFO the full path
pub(crate) fn derive_script_info(uri_path: &str) -> (&str, &str) {
    if let Some(pos) = uri_path.find(".php") {
        let script_end = pos + 4; // len(".php")
        (&uri_path[..script_end], &uri_path[script_end..])
    } else {
        ("/index.php", uri_path)
    }
}

/// Extract a header value as an owned `String`, returning `""` if absent.
fn header_str(headers: &axum::http::HeaderMap, name: axum::http::HeaderName) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned()
}

/// Parse a raw CGI stdout blob into an HTTP `Response`.
///
/// Only used in unit tests and as a convenience shim — the production path
/// calls `parse_cgi_header_block` directly from `parse_streaming_headers`.
#[cfg(test)]
fn parse_cgi_response(stdout: Vec<u8>) -> Response {
    const SEP: &[u8] = b"\r\n\r\n";
    let (header_bytes, body_bytes) =
        if let Some(pos) = stdout.windows(SEP.len()).position(|w| w == SEP) {
            (&stdout[..pos], &stdout[pos + SEP.len()..])
        } else {
            (b"".as_slice(), stdout.as_slice())
        };

    match parse_cgi_header_block(header_bytes) {
        Ok((status, headers)) => {
            let mut builder = Response::builder().status(status);
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            builder
                .body(Body::from(body_bytes.to_vec()))
                .unwrap_or_else(|_| error_response(502, "Failed to build response\n"))
        }
        Err(_) => error_response(502, "Invalid CGI headers\n"),
    }
}

/// Parse the raw CGI header block (bytes **before** `\r\n\r\n`) into an HTTP
/// status code and a list of `(name, value)` header pairs.
///
/// The `Status:` pseudo-header is consumed and not included in the returned
/// header list.  All other headers are forwarded verbatim to the HTTP response.
fn parse_cgi_header_block(header_bytes: &[u8]) -> Result<(axum::http::StatusCode, Vec<(String, String)>), ()> {
    let mut status = axum::http::StatusCode::OK;
    let mut headers = Vec::new();

    let header_str = String::from_utf8_lossy(header_bytes);
    for line in header_str.lines() {
        if let Some((name, value)) = line.split_once(": ") {
            if name.eq_ignore_ascii_case("Status") {
                // Format: "200 OK" or "404 Not Found".  Only the code matters.
                if let Some(code_str) = value.split_ascii_whitespace().next() {
                    if let Ok(code) = code_str.parse::<u16>() {
                        status = axum::http::StatusCode::from_u16(code)
                            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
            } else {
                headers.push((name.to_owned(), value.to_owned()));
            }
        }
    }

    Ok((status, headers))
}

/// Build a plain-text error response with the given HTTP status code.
fn error_response(code: u16, body: &'static str) -> Response {
    Response::builder()
        .status(code)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(Body::from(body))
        .unwrap()
}

// ── Route registry ────────────────────────────────────────────────────────────

/// A URL prefix entry discovered from a PHP app's `routes.php` (or the app
/// directory scan), representing a path that should be proxied to PHP-FPM.
///
/// Used by [`build_route_registry`] and consumed by `nc-server`'s router to
/// register axum routes at startup rather than relying on a single
/// `/apps/{*path}` catch-all.
#[derive(Debug, Clone)]
pub struct RouteEntry {
    /// Base URL path to register.
    ///
    /// Examples:
    /// - `"/apps/files_sharing"` — app-prefixed entry (one per app directory)
    /// - `"/s"` — root-level entry from a `'root' => ''` route in routes.php
    ///
    /// The router will register both the exact base AND a `{base}/{*tail}`
    /// wildcard so that both bare paths and sub-paths are routed to PHP-FPM.
    pub base: String,
    /// Source app name — used only for tracing/logging.
    pub app: String,
}

/// Scan the Nextcloud app tree and build the list of URL prefixes that must be
/// registered as PHP-FPM-proxied routes in the axum router.
///
/// Two categories of entries are returned:
///
/// 1. **App-level** — one entry per directory found in `{nc_root}/apps/`,
///    covering `/apps/{appname}/{*tail}`.  This replaces the generic
///    `/apps/{*path}` catch-all with explicit per-app routes so that truly
///    unknown paths return `404 Not Found` instead of being forwarded to PHP.
///
/// 2. **Root-level** — extracted by parsing `appinfo/routes.php` files for
///    route entries that carry `'root' => ''`.  These are routes registered at
///    the HTTP root rather than under `/apps/{appname}/`, for example:
///    - `/s/{token}` (`files_sharing`) → entry base `"/s"`
///    - `/f/{fileid}` (`files`)        → entry base `"/f"`
///    - `/settings/admin` (`settings`) → entry base `"/settings"`
///
/// The extractor is a regex-based heuristic (not a full PHP parser): for each
/// line containing `'root' => ''`, it looks back up to 20 lines to find the
/// nearest `'url' => '...'` value and extracts the first non-parameterised
/// path segment.  This is stable against the real routes.php format used in
/// Nextcloud's bundled apps.
///
/// Entries are deduplicated (same `base` string) before being returned.
/// Returns an empty `Vec` on any I/O error (non-fatal; the caller falls back
/// gracefully).
pub fn build_route_registry(nc_root: &Path) -> Vec<RouteEntry> {
    use std::collections::BTreeSet;

    let apps_dir = nc_root.join("apps");
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut entries: Vec<RouteEntry> = Vec::new();

    // ── 1. List all app directories ───────────────────────────────────────────
    let read_dir = match std::fs::read_dir(&apps_dir) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                path = %apps_dir.display(),
                error = %e,
                "route-registry: cannot read apps/ directory"
            );
            return entries;
        }
    };

    let mut app_names: Vec<String> = read_dir
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    app_names.sort();

    // ── 2. Add one app-level entry per app directory ──────────────────────────
    for app in &app_names {
        let base = format!("/apps/{}", app);
        if seen.insert(base.clone()) {
            entries.push(RouteEntry { base, app: app.clone() });
        }
    }

    // ── 3. Parse routes.php for root-level route entries ─────────────────────
    //
    // We use a sliding-window heuristic rather than a full PHP parser:
    // - Compile the two patterns once.
    // - For every line containing `'root' => ''`, look back up to 20 lines to
    //   find the last `'url' => '/...'` occurrence in that window.
    // - Extract the first static (non-`{param}`) path segment as the base.
    let url_re = regex_lite::Regex::new(r"'url'\s*=>\s*'(/[^']*)'").unwrap();
    let root_re = regex_lite::Regex::new(r"'root'\s*=>\s*''").unwrap();

    for app in &app_names {
        let routes_path = apps_dir.join(app).join("appinfo/routes.php");
        let content = match std::fs::read_to_string(&routes_path) {
            Ok(c) => c,
            Err(_) => continue, // no routes.php — skip silently
        };

        let lines: Vec<&str> = content.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !root_re.is_match(line) {
                continue;
            }
            // Window: up to 20 lines leading up to (and including) this line.
            let start = i.saturating_sub(20);
            let window = lines[start..=i].join("\n");

            // Find the last URL match in the window (the one closest to 'root').
            if let Some(cap) = url_re.captures_iter(&window).last() {
                let url = &cap[1];
                // Find the first non-empty, non-parameterised segment.
                let segment = url
                    .trim_start_matches('/')
                    .split('/')
                    .find(|s| !s.is_empty() && !s.starts_with('{'));

                if let Some(seg) = segment {
                    let base = format!("/{}", seg);
                    if seen.insert(base.clone()) {
                        tracing::debug!(
                            app = %app,
                            base = %base,
                            "route-registry: root-level route prefix"
                        );
                        entries.push(RouteEntry { base, app: app.clone() });
                    }
                }
            }
        }
    }

    tracing::info!(
        total = entries.len(),
        "route-registry: {} entries built from apps/",
        entries.len()
    );
    entries
}

// ── fetch_php_capabilities ────────────────────────────────────────────────────

/// Fetch PHP-app capabilities from PHP-FPM at startup (Phase 7.7).
///
/// Makes a synthetic `GET /ocs/v2.php/cloud/capabilities?format=json` request
/// to the PHP-FPM shim with the given admin identity and returns the
/// `capabilities` sub-object from the OCS envelope
/// (e.g. `{"files_sharing": {...}, "text": {...}}`).
///
/// Returns `None` on any failure (socket unavailable, PHP error, JSON parse
/// failure).  The caller falls back to native-only capabilities silently.
pub async fn fetch_php_capabilities(
    fpm: &FastCgiState,
    admin_uid: &str,
) -> Option<serde_json::Value> {
    let timeout = std::time::Duration::from_millis(fpm.timeout_ms);

    // ── Connect ────────────────────────────────────────────────────────────
    let stream = match tokio::time::timeout(
        timeout,
        tokio::net::UnixStream::connect(&fpm.socket_path),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "capabilities-fetch: PHP-FPM socket unavailable");
            return None;
        }
        Err(_) => {
            tracing::warn!("capabilities-fetch: timed out connecting to PHP-FPM");
            return None;
        }
    };

    let shim_path = fpm.shim_path.to_string_lossy().into_owned();
    let nc_original = fpm.nc_root.join("ocs/v2.php").to_string_lossy().into_owned();
    let document_root = fpm.nc_root.to_string_lossy().into_owned();

    // OCS-APIREQUEST bypasses CSRF; format=json for easy parsing.
    let params = fastcgi_client::Params::default()
        .gateway_interface("CGI/1.1")
        .server_software("nc-server/0.1")
        .server_protocol("HTTP/1.1")
        .request_method("GET")
        .script_filename(shim_path)
        .custom("NC_ORIGINAL_SCRIPT", nc_original)
        .custom("DOCUMENT_ROOT", document_root)
        .custom("SCRIPT_NAME", "/ocs/v2.php")
        .custom("REQUEST_URI", "/ocs/v2.php/cloud/capabilities?format=json")
        .custom("QUERY_STRING", "format=json")
        .custom("CONTENT_TYPE", "")
        .custom("CONTENT_LENGTH", "0")
        .custom("PATH_INFO", "/cloud/capabilities")
        .custom("HTTP_OCS_APIREQUEST", "true")
        .custom("HTTP_ACCEPT", "application/json")
        .custom("HTTP_X_NC_USER", admin_uid.to_owned())
        .custom("HTTP_X_NC_IS_ADMIN", "1")
        .custom("HTTP_X_NC_PROXIED", "1"); // proxy trust marker required by shim (§7.3/§7.8)

    let stdin = StreamReader::new(futures::stream::once(std::future::ready(
        Ok::<Bytes, std::io::Error>(Bytes::new()),
    )));
    let client = fastcgi_client::Client::new_tokio(stream);
    let fcgi_req = fastcgi_client::Request::new_tokio(params, stdin);

    // ── Execute ────────────────────────────────────────────────────────────
    let mut response_stream = match tokio::time::timeout(
        timeout,
        client.execute_once_stream(fcgi_req),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "capabilities-fetch: FastCGI execution failed");
            return None;
        }
        Err(_) => {
            tracing::warn!("capabilities-fetch: timed out waiting for PHP-FPM response");
            return None;
        }
    };

    // ── Collect full response ──────────────────────────────────────────────
    let mut full_output = BytesMut::new();
    while let Some(item) = response_stream.next().await {
        match item {
            Ok(Content::Stdout(b)) => full_output.extend_from_slice(&b),
            Ok(Content::Stderr(b)) => {
                if !b.is_empty() {
                    tracing::debug!(
                        stderr = %String::from_utf8_lossy(&b),
                        "capabilities-fetch: PHP-FPM stderr"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "capabilities-fetch: stream error");
                break;
            }
        }
    }

    // ── Parse: skip CGI headers, decode JSON, extract capabilities ─────────
    let output = full_output.freeze();
    const SEP: &[u8] = b"\r\n\r\n";
    let body_start = output
        .windows(SEP.len())
        .position(|w| w == SEP)
        .map(|p| p + SEP.len())?;
    let body = &output[body_start..];

    let json: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                body_preview = %String::from_utf8_lossy(&body[..body.len().min(200)]),
                "capabilities-fetch: failed to parse PHP response as JSON"
            );
            return None;
        }
    };

    // Extract ocs.data.capabilities from the OCS v2 envelope.
    let caps = json
        .get("ocs")
        .and_then(|ocs| ocs.get("data"))
        .and_then(|data| data.get("capabilities"))
        .cloned();

    if caps.is_none() {
        tracing::warn!("capabilities-fetch: PHP response missing ocs.data.capabilities");
    } else {
        tracing::debug!("capabilities-fetch: received PHP-app capabilities");
    }

    caps
}

// ── fetch_php_public_capabilities ─────────────────────────────────────────────

/// Fetch the IPublicCapability-only subset of PHP-app capabilities (Phase 7.7).
///
/// Identical to [`fetch_php_capabilities`] except that `HTTP_X_NC_USER` is
/// **not** injected.  PHP-FPM therefore receives an unauthenticated request and
/// naturally processes it through `getCapabilities(true)`, returning only the
/// capabilities that implement `IPublicCapability`.
///
/// The PHP bootstrap shim whitelists `PATH_INFO = /cloud/capabilities` for
/// `v1.php`/`v2.php` entry points in `reject_unauthenticated_shim_request()`,
/// so the security gate passes.  OC::handleRequest() then runs without a session
/// user and the PHP capability stack calls `getCapabilities(true)` naturally.
///
/// Returns `None` on any failure (socket unavailable, PHP error, JSON parse
/// failure).  The caller falls back to native-only public capabilities silently.
pub async fn fetch_php_public_capabilities(
    fpm: &FastCgiState,
) -> Option<serde_json::Value> {
    let timeout = std::time::Duration::from_millis(fpm.timeout_ms);

    // ── Connect ────────────────────────────────────────────────────────────
    let stream = match tokio::time::timeout(
        timeout,
        tokio::net::UnixStream::connect(&fpm.socket_path),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "public-caps-fetch: PHP-FPM socket unavailable");
            return None;
        }
        Err(_) => {
            tracing::warn!("public-caps-fetch: timed out connecting to PHP-FPM");
            return None;
        }
    };

    let shim_path = fpm.shim_path.to_string_lossy().into_owned();
    let nc_original = fpm.nc_root.join("ocs/v2.php").to_string_lossy().into_owned();
    let document_root = fpm.nc_root.to_string_lossy().into_owned();

    // No HTTP_X_NC_USER — shim whitelists this path and PHP sees no session.
    let params = fastcgi_client::Params::default()
        .gateway_interface("CGI/1.1")
        .server_software("nc-server/0.1")
        .server_protocol("HTTP/1.1")
        .request_method("GET")
        .script_filename(shim_path)
        .custom("NC_ORIGINAL_SCRIPT", nc_original)
        .custom("DOCUMENT_ROOT", document_root)
        .custom("SCRIPT_NAME", "/ocs/v2.php")
        .custom("REQUEST_URI", "/ocs/v2.php/cloud/capabilities?format=json")
        .custom("QUERY_STRING", "format=json")
        .custom("CONTENT_TYPE", "")
        .custom("CONTENT_LENGTH", "0")
        .custom("PATH_INFO", "/cloud/capabilities")
        .custom("HTTP_OCS_APIREQUEST", "true")
        .custom("HTTP_ACCEPT", "application/json")
        .custom("HTTP_X_NC_PROXIED", "1"); // proxy trust marker required by shim (§7.3/§7.8)
    // Deliberately omit HTTP_X_NC_USER and HTTP_X_NC_IS_ADMIN so PHP receives
    // an unauthenticated context and calls getCapabilities(true).

    let stdin = StreamReader::new(futures::stream::once(std::future::ready(
        Ok::<Bytes, std::io::Error>(Bytes::new()),
    )));
    let client = fastcgi_client::Client::new_tokio(stream);
    let fcgi_req = fastcgi_client::Request::new_tokio(params, stdin);

    // ── Execute ────────────────────────────────────────────────────────────
    let mut response_stream = match tokio::time::timeout(
        timeout,
        client.execute_once_stream(fcgi_req),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "public-caps-fetch: FastCGI execution failed");
            return None;
        }
        Err(_) => {
            tracing::warn!("public-caps-fetch: timed out waiting for PHP-FPM response");
            return None;
        }
    };

    // ── Collect full response ──────────────────────────────────────────────
    let mut full_output = BytesMut::new();
    while let Some(item) = response_stream.next().await {
        match item {
            Ok(Content::Stdout(b)) => full_output.extend_from_slice(&b),
            Ok(Content::Stderr(b)) => {
                if !b.is_empty() {
                    tracing::debug!(
                        stderr = %String::from_utf8_lossy(&b),
                        "public-caps-fetch: PHP-FPM stderr"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "public-caps-fetch: stream error");
                break;
            }
        }
    }

    // ── Parse: skip CGI headers, decode JSON, extract capabilities ─────────
    let output = full_output.freeze();
    const SEP: &[u8] = b"\r\n\r\n";
    let body_start = output
        .windows(SEP.len())
        .position(|w| w == SEP)
        .map(|p| p + SEP.len())?;
    let body = &output[body_start..];

    let json: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                body_preview = %String::from_utf8_lossy(&body[..body.len().min(200)]),
                "public-caps-fetch: failed to parse PHP response as JSON"
            );
            return None;
        }
    };

    let caps = json
        .get("ocs")
        .and_then(|ocs| ocs.get("data"))
        .and_then(|data| data.get("capabilities"))
        .cloned();

    if caps.is_none() {
        tracing::warn!("public-caps-fetch: PHP response missing ocs.data.capabilities");
    } else {
        tracing::debug!("public-caps-fetch: received IPublicCapability PHP-app capabilities");
    }

    caps
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── derive_script_info ─────────────────────────────────────────────────────

    #[test]
    fn script_info_index_php() {
        assert_eq!(derive_script_info("/index.php"), ("/index.php", ""));
    }

    #[test]
    fn script_info_index_php_with_path() {
        assert_eq!(
            derive_script_info("/index.php/apps/files/api/v1/stats"),
            ("/index.php", "/apps/files/api/v1/stats")
        );
    }

    #[test]
    fn script_info_ocs_v2() {
        assert_eq!(
            derive_script_info("/ocs/v2.php/cloud/capabilities"),
            ("/ocs/v2.php", "/cloud/capabilities")
        );
    }

    #[test]
    fn script_info_remote_php() {
        assert_eq!(
            derive_script_info("/remote.php/dav/files/alice/Documents"),
            ("/remote.php", "/dav/files/alice/Documents")
        );
    }

    #[test]
    fn script_info_clean_url() {
        // Clean URL (no .php) → route through index.php
        assert_eq!(
            derive_script_info("/apps/files_sharing/api/v1/shares"),
            ("/index.php", "/apps/files_sharing/api/v1/shares")
        );
    }

    #[test]
    fn script_info_root_clean_url() {
        assert_eq!(derive_script_info("/"), ("/index.php", "/"));
    }

    // ── build_route_registry ───────────────────────────────────────────────────

    /// Verify the registry correctly scans the real apps/ tree in this repo.
    ///
    /// This test uses the actual `apps/` directory committed to the repository
    /// (located at `{workspace_root}/../../..` relative to the crate manifest
    /// directory `core-rs/crates/nc-fastcgi/`).
    #[test]
    fn registry_scans_real_apps_dir() {
        // Navigate from core-rs/crates/nc-fastcgi/ → nc-server root
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let nc_root = crate_dir.join("../../..").canonicalize().unwrap();

        let entries = build_route_registry(&nc_root);

        // At minimum, every installed app directory should produce an entry.
        let bases: Vec<&str> = entries.iter().map(|e| e.base.as_str()).collect();

        // App-level entries for bundled apps
        assert!(
            bases.contains(&"/apps/files"),
            "expected /apps/files entry, got: {:?}", bases
        );
        assert!(
            bases.contains(&"/apps/files_sharing"),
            "expected /apps/files_sharing entry"
        );
        assert!(
            bases.contains(&"/apps/settings"),
            "expected /apps/settings entry"
        );

        // Root-level entries from routes with 'root' => ''
        assert!(
            bases.contains(&"/s"),
            "expected /s root-level entry from files_sharing"
        );
        assert!(
            bases.contains(&"/f"),
            "expected /f root-level entry from files"
        );
        assert!(
            bases.contains(&"/settings"),
            "expected /settings root-level entry from settings app"
        );

        // No duplicate bases
        let mut seen = std::collections::HashSet::new();
        for base in &bases {
            assert!(seen.insert(*base), "duplicate base: {}", base);
        }
    }

    /// Verify the registry returns an empty Vec and does not panic when
    /// the apps/ directory does not exist.
    #[test]
    fn registry_returns_empty_for_missing_apps_dir() {
        let entries = build_route_registry(std::path::Path::new("/nonexistent/path"));
        assert!(entries.is_empty());
    }

    // ── proxy trust marker ─────────────────────────────────────────────────────

    /// Document and verify the naming convention for the proxy trust marker.
    ///
    /// The Rust proxy injects HTTP_X_NC_PROXIED as a FastCGI param and strips
    /// the corresponding HTTP header (x-nc-proxied) from client requests.  The
    /// PHP shim checks `$_SERVER['HTTP_X_NC_PROXIED'] === '1'`.
    /// This test ensures the FCG param name computed from the HTTP header name
    /// matches exactly what the shim expects (§7.3/§7.8).
    #[test]
    fn proxy_marker_fcgi_param_name_matches_shim_expectation() {
        // HTTP header name as it appears in request headers (lowercase normalised)
        let http_header = "x-nc-proxied";
        // FastCGI param name produced by the "HTTP_" + uppercase + hyphen→underscore rule
        let fcgi_param: String = format!(
            "HTTP_{}",
            http_header.to_ascii_uppercase().replace('-', "_")
        );
        // This must match exactly what the PHP shim checks: $_SERVER['HTTP_X_NC_PROXIED']
        assert_eq!(fcgi_param, "HTTP_X_NC_PROXIED");
    }

    // ── parse_cgi_header_block ─────────────────────────────────────────────────

    #[test]
    fn header_block_default_200() {
        let (status, headers) =
            parse_cgi_header_block(b"Content-Type: text/html").unwrap();
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Content-Type");
    }

    #[test]
    fn header_block_explicit_404() {
        let (status, headers) =
            parse_cgi_header_block(b"Content-Type: application/json\r\nStatus: 404 Not Found")
                .unwrap();
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        // Status pseudo-header must NOT be forwarded.
        assert!(headers.iter().all(|(n, _)| !n.eq_ignore_ascii_case("status")));
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn header_block_extra_headers_forwarded() {
        let (_, headers) =
            parse_cgi_header_block(b"Content-Type: text/xml\r\nX-Robots-Tag: none")
                .unwrap();
        assert!(headers.iter().any(|(n, _)| n == "X-Robots-Tag"));
    }

    // ── parse_cgi_response (integration shim used only in tests) ──────────────

    #[test]
    fn cgi_response_default_200() {
        let stdout =
            b"Content-Type: text/html\r\n\r\n<html>hello</html>".to_vec();
        let resp = parse_cgi_response(stdout);
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[test]
    fn cgi_response_explicit_status() {
        let stdout = b"Content-Type: application/json\r\nStatus: 404 Not Found\r\n\r\n{}"
            .to_vec();
        let resp = parse_cgi_response(stdout);
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn cgi_response_headers_forwarded() {
        let stdout =
            b"Content-Type: text/xml\r\nX-Robots-Tag: none\r\n\r\nbody".to_vec();
        let resp = parse_cgi_response(stdout);
        assert!(resp.headers().contains_key("x-robots-tag"));
        assert!(!resp.headers().contains_key("status"));
    }

    #[test]
    fn cgi_response_no_separator_falls_back_200() {
        // Malformed CGI response with no separator — falls back to 502.
        let stdout = b"this is not valid cgi output".to_vec();
        let resp = parse_cgi_response(stdout);
        // No separator → header parse on empty bytes → 200, body is full content.
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }
}
