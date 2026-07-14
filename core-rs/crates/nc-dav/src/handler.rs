//! Axum handler for DAV routes.
//!
//! A single `dav_handler` function is registered for all DAV mount points:
//! - `/remote.php/webdav/{*path}` (legacy desktop sync alias)
//! - `/remote.php/dav/{*path}`
//! - `/dav/files/{uid}/{*path}` (per-user WebDAV)
//! - `/dav/{*path}` (general DAV namespace)
//!
//! For each request it:
//! 1. Reads authenticated user from the `AuthInfo` extension.
//! 2. Extracts `X-OC-MTime` and `X-OC-CTime` headers before forwarding.
//! 3. Resolves the user's home storage ID.
//! 4. Builds a per-request `NcFileSystem` and wraps it in a `DavHandler`.
//! 5. Delegates to `dav_handler.handle(req)` and post-processes the response
//!    to inject Nextcloud-specific headers (REQ §6.4, §13.2, §15.1).

use std::sync::Arc;

use axum::extract::Request;
use axum::response::Response;
use axum::{body::Body, extract::State};
use dav_server::DavConfig;
use http::{HeaderName, HeaderValue, Method, StatusCode};
use nc_auth::AuthInfo;

use crate::{filesystem::NcFileSystem, locksystem::NcLockSystem, row, NcDavState};
use nc_db::FilenameError;

// ─── Header name constants ───────────────────────────────────────────────────

static H_CSP: HeaderName = HeaderName::from_static("content-security-policy");
static H_OC_FILEID: HeaderName = HeaderName::from_static("oc-fileid");
static H_OC_ETAG: HeaderName = HeaderName::from_static("oc-etag");
static H_OC_CHECKSUM: HeaderName = HeaderName::from_static("oc-checksum");
static H_X_OC_MTIME: HeaderName = HeaderName::from_static("x-oc-mtime");
static H_X_OC_CTIME: HeaderName = HeaderName::from_static("x-oc-ctime");
static H_X_ACCEL_BUFFERING: HeaderName = HeaderName::from_static("x-accel-buffering");
static H_X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
static H_X_NC_USER_ID: HeaderName = HeaderName::from_static("x-nextcloud-user-id");

// ─── Public handler ───────────────────────────────────────────────────────────

/// Axum handler function for all DAV endpoints.
pub async fn dav_handler(State(state): State<NcDavState>, req: Request) -> Response {
    // ── Extract NC-specific headers before consuming the request ──────────
    let x_oc_mtime: Option<i64> = req
        .headers()
        .get("x-oc-mtime")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());
    let x_oc_ctime: Option<i64> = req
        .headers()
        .get("x-oc-ctime")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    // 4.14.7 RequestIdHeaderPlugin: use incoming X-Request-Id or generate a UUID.
    let request_id: String = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // §5.1 — Destination header needed for MOVE/COPY filename validation.
    let destination_header: Option<String> = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // §5.2 — Upload size for quota enforcement.
    // Take the maximum of all three size hints (REQ §7.1).
    let max_upload_bytes: i64 = [
        req.headers().get(http::header::CONTENT_LENGTH),
        req.headers().get("x-expected-entity-length"),
        req.headers().get("oc-total-length"),
    ]
    .iter()
    .filter_map(|h| {
        h.and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|&n| n >= 0)
    })
    .max()
    .unwrap_or(0);

    // Save method and path for post-response processing
    let req_method = req.method().clone();
    let req_path = req.uri().path().to_string();

    // ── Resolve authenticated user ────────────────────────────────────────
    let uid = match req.extensions().get::<AuthInfo>() {
        Some(info) => info.uid.clone(),
        None => {
            return http::Response::builder()
                .status(401)
                .header("WWW-Authenticate", "Basic realm=\"Nextcloud\"")
                .body(Body::from("Unauthorized"))
                .unwrap();
        }
    };

    // ── Determine strip prefix ────────────────────────────────────────────
    let path = req.uri().path();
    let strip_prefix = determine_prefix(path, &uid);

    // ── §5.1 Filename validation (write methods) ──────────────────────────
    //
    // Check the target filename BEFORE issuing any DB query so that invalid
    // names fail immediately with 422 Unprocessable Entity.
    //
    // Covered methods (per REQ §7 / IMPL_PLAN §5):
    //   PUT    — last segment of the request path
    //   MKCOL  — last segment of the request path
    //   MOVE   — last segment of the Destination header
    //   COPY   — last segment of the Destination header
    if let Some(name) = write_target_name(
        &req_method,
        &req_path,
        &strip_prefix,
        destination_header.as_deref(),
    ) {
        if let Err(e) = state.filename_validator.validate(&name) {
            tracing::debug!(
                method = %req_method, path = %req_path, name = %name,
                reason = %e, "§5.1 filename validation rejected"
            );
            return build_filename_error_response(e);
        }
    }

    // ── Resolve home storage ID ───────────────────────────────────────────
    let data_dir_str = state.data_directory.to_str().unwrap_or("").to_string();

    let storage_id =
        match row::lookup_storage_id(&state.pool, &state.table_prefix, &uid, &data_dir_str).await {
            Some(id) => id,
            None => {
                tracing::warn!(uid = %uid, "DAV: home storage not found in oc_storages");
                return http::Response::builder()
                    .status(503)
                    .body(Body::from("Storage not available"))
                    .unwrap();
            }
        };

    // ── §5.2 Quota enforcement (PUT) ─────────────────────────────────────
    //
    // Checked for PUT with a known upload size.  `max_upload_bytes` holds the
    // maximum of Content-Length, X-Expected-Entity-Length, and OC-Total-Length.
    //
    // Any negative free_space() return value → skip (unlimited / unknown quota).
    // Quota exceeded → 507 Insufficient Storage (REQ §7.1, PHASE-5.2).
    if req_method == Method::PUT {
        if let Err(()) = crate::quota::check_quota(
            &state.pool,
            &state.table_prefix,
            &state.appconfig_cache,
            &uid,
            storage_id,
            max_upload_bytes,
        )
        .await
        {
            let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                        <d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n  \
                        <s:exception>OCA\\DAV\\Connector\\Sabre\\Exception\\InsufficientStorage\
</s:exception>\n  \
                        <s:message>Quota exceeded: insufficient free space to upload.</s:message>\n\
                        </d:error>\n";
            let mut resp = http::Response::builder()
                .status(StatusCode::INSUFFICIENT_STORAGE)
                .header(H_CSP.clone(), HeaderValue::from_static("default-src 'none';"))
                .header(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/xml; charset=utf-8"),
                )
                .body(Body::from(body))
                .unwrap();
            if let Ok(v) = HeaderValue::from_str(&request_id) {
                resp.headers_mut().insert(H_X_REQUEST_ID.clone(), v);
            }
            return resp;
        }
    }

    // ── §5.4 OC-Chunked v1: return 501 ────────────────────────────────────
    //
    // OC-Chunked was an older desktop sync client protocol (pre-3.0, 2020).
    // Requests with the `OC-Chunked: 1` header return `501 Not Implemented`.
    if req.headers().get("oc-chunked").is_some() {
        tracing::info!(
            method = %req_method,
            path = %req_path,
            "§5.4 OC-Chunked v1 request — returning 501"
        );
        let mut resp = http::Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .header(H_CSP.clone(), HeaderValue::from_static("default-src 'none';"))
            .body(Body::from("OC-Chunked v1 is not supported. Use chunked upload v2 or simple PUT."))
            .unwrap();
        if let Ok(v) = HeaderValue::from_str(&request_id) {
            resp.headers_mut().insert(H_X_REQUEST_ID.clone(), v);
        }
        return resp;
    }

    // ── 4.14.6 DummyGetResponsePlugin ────────────────────────────────────
    //
    // GET on a DAV collection returns a plain-text stub instead of an HTML
    // directory listing (REQ §14.6).  We check pre-dispatch so that normal
    // file GETs (handled by dav-server via `get_file`) are unaffected.
    if req_method == Method::GET {
        let dav_path = req_path
            .strip_prefix(strip_prefix.as_str())
            .unwrap_or("/");
        let fc_path = crate::row::dav_to_fc_path(dav_path);

        // ── §5.10 ZIP/TAR folder download ────────────────────────────────
        //
        // Before the generic GET-on-directory handler (DummyGetResponsePlugin),
        // check if the client asked for an archive format.  If so, stream the
        // folder as a ZIP or TAR and return immediately.
        //
        // Mirrors PHP ZipFolderPlugin (apps/dav/.../ZipFolderPlugin.php).
        {
            let accept_header = req
                .headers()
                .get("accept")
                .and_then(|v| v.to_str().ok());
            let uri_query = req.uri().query();

            let x_nc_files: Vec<String> = req
                .headers()
                .get_all("x-nc-files")
                .iter()
                .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
                .collect();

            if let Some(archive_resp) = crate::archive::try_serve_archive(
                &state.pool,
                &state.table_prefix,
                storage_id,
                &fc_path,
                &state.data_directory,
                &uid,
                &state.mime_cache,
                &state.appconfig_cache,
                accept_header,
                uri_query,
                x_nc_files,
                &request_id,
            )
            .await
            {
                tracing::debug!(fc_path, "§5.10 archive download served");
                return archive_resp;
            }
        }

        if let Some(fc_row) =
            crate::row::lookup_by_path(&state.pool, &state.table_prefix, storage_id, &fc_path)
                .await
        {
            let is_dir = {
                let cache = state.mime_cache.read().expect("mime cache lock");
                cache
                    .get_name(fc_row.mimetype)
                    .map_or(false, |m| m == "httpd/unix-directory")
            };
            if is_dir {
                let mut resp = http::Response::builder()
                    .status(200)
                    .header(H_CSP.clone(), HeaderValue::from_static("default-src 'none';"))
                    .header(
                        http::header::CONTENT_TYPE,
                        HeaderValue::from_static("text/plain; charset=utf-8"),
                    )
                    .body(Body::from(
                        "This is the WebDAV interface. \
                         It can only be accessed by WebDAV clients \
                         such as the Nextcloud desktop sync client.",
                    ))
                    .unwrap();
                // Also inject tracing headers on the early return.
                if let Ok(v) = HeaderValue::from_str(&request_id) {
                    resp.headers_mut().insert(H_X_REQUEST_ID.clone(), v);
                }
                if let Ok(v) = HeaderValue::from_str(&uid) {
                    resp.headers_mut().insert(H_X_NC_USER_ID.clone(), v);
                }
                return resp;
            }
        }
    }

    // ── Intercept REPORT (RFC 6578 sync-collection, PHASE-4.11) ──────────
    //
    // dav-server-rs does not implement REPORT.  We handle sync-collection
    // natively here and return 501 for any other REPORT type.  This must
    // happen before `state` is moved into the per-request NcFileSystem.
    if req_method.as_str() == "REPORT" {
        let body_bytes = axum::body::to_bytes(req.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap_or_default();
        let sync_req = crate::sync::parse_report_body(&body_bytes);
        if sync_req.is_sync_collection {
            let fc_base = crate::row::dav_to_fc_path(
                req_path
                    .strip_prefix(strip_prefix.as_str())
                    .unwrap_or("/"),
            );
            return crate::sync::build_sync_response(
                &state,
                &uid,
                storage_id,
                &strip_prefix,
                &fc_base,
                sync_req.since_mtime,
            )
            .await;
        }
        // Non-sync-collection REPORT types are not implemented on the file tree.
        return http::Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .header(H_CSP.clone(), HeaderValue::from_static("default-src 'none';"))
            .body(Body::from("Not Implemented"))
            .unwrap();
    }

    // Clone what we need for post-response steps (state is moved below).
    let pool_ref = state.pool.clone();
    let prefix_ref = state.table_prefix.clone();
    let strip_ref = strip_prefix.clone();
    let mime_cache_ref = state.mime_cache.clone();

    // ── Intercept DELETE for directories ──────────────────────────────────
    //
    // dav-server-rs recursively walks children before calling remove_dir,
    // which would trash each file individually.  PHP's SabreDAV handles
    // directory deletes atomically via View::rmdir() → Trashbin::move2trash().
    // We intercept here before the framework to match that behaviour.
    if req_method == Method::DELETE {
        let dav_path = req_path
            .strip_prefix(strip_prefix.as_str())
            .unwrap_or(&req_path);
        // Percent-decode: the request URI may have %20 etc., but the
        // filecache stores the decoded path.
        let decoded = percent_decode_path(dav_path.trim_end_matches('/'));
        let fc_path = crate::row::dav_to_fc_path(&decoded);

        // Check if the target is a directory.
        if let Some(fc_row) =
            crate::row::lookup_by_path(&pool_ref, &prefix_ref, storage_id, &fc_path).await
        {
            let dir_mime_id = {
                let cache = state.mime_cache.read().expect("mime cache lock");
                cache.get_id("httpd/unix-directory")
            };
            if Some(fc_row.mimetype) == dir_mime_id {
                // Check if trashbin is enabled before intercepting.
                let trash_enabled = {
                    let sql = format!(
                        "SELECT configvalue FROM {prefix}appconfig \
                         WHERE appid = 'files_trashbin' AND configkey = 'enabled'",
                        prefix = prefix_ref
                    );
                    match sqlx::query_scalar::<_, String>(&sql)
                        .fetch_optional(&pool_ref)
                        .await
                    {
                        Ok(Some(val)) => val == "yes" || val == "true",
                        _ => false,
                    }
                };

                if trash_enabled {
                    let write_result: crate::SharedWriteResult =
                        Arc::new(std::sync::Mutex::new(None));
                    let put_error: crate::SharedPutError =
                        Arc::new(std::sync::Mutex::new(None));
                    let fs = NcFileSystem::new(
                        state,
                        uid.clone(),
                        storage_id,
                        x_oc_mtime,
                        x_oc_ctime,
                        write_result,
                        put_error,
                    );

                    match fs.trash_directory(&fc_path).await {
                        Ok(()) => {
                            return http::Response::builder()
                                .status(StatusCode::NO_CONTENT)
                                .header(H_CSP.clone(), HeaderValue::from_static("default-src 'none';"))
                                .body(Body::empty())
                                .unwrap();
                        }
                        Err(e) => {
                            let status = match e {
                                dav_server::fs::FsError::NotFound => StatusCode::NOT_FOUND,
                                dav_server::fs::FsError::Forbidden => StatusCode::FORBIDDEN,
                                _ => StatusCode::INTERNAL_SERVER_ERROR,
                            };
                            return http::Response::builder()
                                .status(status)
                                .header(H_CSP.clone(), HeaderValue::from_static("default-src 'none';"))
                                .body(Body::empty())
                                .unwrap();
                        }
                    }
                }
            }
        }
    }

    // ── Build per-request filesystem ──────────────────────────────────────
    let write_result: crate::SharedWriteResult = Arc::new(std::sync::Mutex::new(None));
    let put_error: crate::SharedPutError = Arc::new(std::sync::Mutex::new(None));

    let fs = NcFileSystem::new(
        state,
        uid.clone(),
        storage_id,
        x_oc_mtime,
        x_oc_ctime,
        write_result.clone(),
        put_error.clone(),
    );

    let handler = DavConfig::new()
        .filesystem(Box::new(fs))
        .locksystem(NcLockSystem::new())
        .strip_prefix(strip_prefix)
        .principal(uid.clone())
        .build_handler();

    // ── Delegate to dav-server ────────────────────────────────────────────
    let dav_resp = handler.handle(req).await;
    let (mut parts, dav_body) = dav_resp.into_parts();

    // ── Post-process: inject Nextcloud response headers ───────────────────
    // 0. Rewrite 500 → 400 if a PUT failed due to a known client error such
    //    as a checksum mismatch (REQ §13.1 / PHASE-4.4).  dav-server has no
    //    BadRequest FsError variant; `flush()` returns GeneralFailure (500)
    //    and communicates the real cause via the put_error side-channel.
    if req_method == Method::PUT && parts.status == StatusCode::INTERNAL_SERVER_ERROR {
        if let Ok(guard) = put_error.lock() {
            if *guard == Some(crate::PutErrorKind::ChecksumMismatch) {
                parts.status = StatusCode::BAD_REQUEST;
            }
        }
    }
    // 1. Content-Security-Policy on every DAV response (REQ §2.4 / §15.1)
    parts.headers.insert(
        H_CSP.clone(),
        HeaderValue::from_static("default-src 'none';"),
    );

    // 2. X-Accel-Buffering: no on GET 2xx (REQ §6.4) — prevents nginx buffering
    if req_method == Method::GET && parts.status.is_success() {
        parts
            .headers
            .insert(H_X_ACCEL_BUFFERING.clone(), HeaderValue::from_static("no"));
    }

    // 3. Write-response headers: OC-FileId, ETag, OC-ETag, X-OC-MTime/CTime (REQ §6.4 / §7.1)
    //
    // `flush()` writes the new ETag into both `self.meta` (for dav-server's
    // post-flush `file.metadata()` call) and into `WriteResult` here.
    // Using `insert` (not `entry().or_insert`) ensures any stale ETag set by
    // dav-server from the pre-write metadata is overwritten with the fresh one.
    if let Ok(guard) = write_result.lock() {
        if let Some(ref wr) = *guard {
            if let Ok(v) = HeaderValue::from_str(&wr.fileid.to_string()) {
                parts.headers.insert(H_OC_FILEID.clone(), v);
            }
            // ETag: quoted, per RFC 4918 §8.8 (PHASE-5.3)
            let quoted_etag = format!("\"{}\"", wr.etag);
            if let Ok(v) = HeaderValue::from_str(&quoted_etag) {
                parts.headers.insert(http::header::ETAG, v);
            }
            // OC-ETag: unquoted mirror of ETag (REQ §6.4)
            if let Ok(v) = HeaderValue::from_str(&wr.etag) {
                parts.headers.insert(H_OC_ETAG.clone(), v);
            }
            if wr.mtime_accepted {
                parts
                    .headers
                    .insert(H_X_OC_MTIME.clone(), HeaderValue::from_static("accepted"));
            }
            if wr.ctime_accepted {
                parts
                    .headers
                    .insert(H_X_OC_CTIME.clone(), HeaderValue::from_static("accepted"));
            }
        }
    }

    // 4. Mirror ETag → OC-ETag on all responses that carry an ETag (REQ §14.4)
    // 4.14.7 RequestIdHeaderPlugin: propagate X-Request-Id on every response (REQ §14.3 / §17)
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        parts.headers.insert(H_X_REQUEST_ID.clone(), v);
    }
    // 4.14.8 UserIdHeaderPlugin: X-Nextcloud-User-Id on every authenticated response (REQ §14.3)
    if let Ok(v) = HeaderValue::from_str(&uid) {
        parts.headers.insert(H_X_NC_USER_ID.clone(), v);
    }

    if let Some(etag_val) = parts.headers.get(http::header::ETAG).cloned() {
        // OC-ETag is the ETag value without quotes
        let oc = etag_val.to_str().unwrap_or("").trim_matches('"');
        if let Ok(v) = HeaderValue::from_str(oc) {
            parts.headers.entry(H_OC_ETAG.clone()).or_insert(v);
        }
    }

    // 5. OC-Checksum on GET 200 (REQ §13.2)
    //    Also override Content-Type with the DB-stored MIME type (REQ §4.2):
    //    NcMetaData::content_type() is the authoritative accessor; here we
    //    resolve it from the mime cache using the numeric mimetype id from the
    //    filecache row, matching what content_type() would return on a loaded
    //    NcMetaData instance.
    if req_method == Method::GET && parts.status == StatusCode::OK {
        let dav_path = req_path
            .strip_prefix(strip_ref.as_str())
            .unwrap_or(&req_path);
        let fc_path = crate::row::dav_to_fc_path(dav_path);
        if let Some(fc_row) =
            crate::row::lookup_by_path(&pool_ref, &prefix_ref, storage_id, &fc_path).await
        {
            // OC-Checksum header
            if let Some(cs) = fc_row.checksum.filter(|s| !s.is_empty()) {
                if let Ok(v) = HeaderValue::from_str(&cs) {
                    parts.headers.insert(H_OC_CHECKSUM.clone(), v);
                }
            }
            // Content-Type: authoritative stored MIME from oc_mimetypes
            let mime_str = {
                let cache = mime_cache_ref.read().expect("mime cache lock");
                cache
                    .get_name(fc_row.mimetype)
                    .unwrap_or("application/octet-stream")
                    .to_string()
            };
            if let Ok(v) = HeaderValue::from_str(&mime_str) {
                parts.headers.insert(http::header::CONTENT_TYPE, v);
            }
            // Content-Disposition: attachment for file downloads (REQ §6.4)
            //
            // Matches PHP FilesPlugin::httpGet() ($downloadAttachment = true):
            //   attachment; filename*=UTF-8''<rawurlencode>; filename="<rawurlencode>"
            //
            // Only applied to files (not directories).  Only set when no
            // Content-Disposition header was already present (dav-server does
            // not set one, so this is always a fresh insert in practice).
            if mime_str != "httpd/unix-directory" {
                let file_name = fc_path
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("download");
                let encoded = percent_encode_filename(file_name);
                let cd = format!(
                    "attachment; filename*=UTF-8''{encoded}; filename=\"{encoded}\""
                );
                if let Ok(cd_val) = HeaderValue::from_str(&cd) {
                    parts
                        .headers
                        .entry(http::header::CONTENT_DISPOSITION)
                        .or_insert(cd_val);
                }
            }
        }
    }

    Response::from_parts(parts, Body::new(dav_body))
}

// ─── §5.1 Filename validation helpers ────────────────────────────────────────

/// Extract the filename component to validate before a write operation.
///
/// - `PUT` / `MKCOL`: last segment of the request path (after stripping the DAV prefix).
/// - `MOVE` / `COPY`: last segment of the path extracted from the `Destination` header.
///
/// Returns `None` for non-write methods or when no name can be determined.
fn write_target_name(
    method: &Method,
    req_path: &str,
    strip_prefix: &str,
    destination: Option<&str>,
) -> Option<String> {
    match method.as_str() {
        "PUT" | "MKCOL" => {
            let dav_path = req_path
                .strip_prefix(strip_prefix)
                .unwrap_or(req_path);
            let decoded = percent_decode_path(dav_path.trim_end_matches('/'));
            let name = decoded.rsplit('/').next().unwrap_or("").to_string();
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        }
        "MOVE" | "COPY" => {
            let dest = destination?;
            let path_part = url_to_path(dest);
            let decoded = percent_decode_path(path_part.trim_end_matches('/'));
            let name = decoded.rsplit('/').next().unwrap_or("").to_string();
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        }
        _ => None,
    }
}

/// Percent-decode a URL path component, producing lossy-UTF-8 output.
///
/// Handles multi-byte sequences (`%C3%A9` → `é`) by collecting every decoded
/// byte and then converting with `String::from_utf8_lossy`.
fn percent_decode_path(path: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) =
                (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2]))
            {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Extract the path component from a `Destination` header value.
///
/// If the value is a full URL (`http://host/path`), strips the scheme and
/// authority to return only the path.  Otherwise returns the value as-is.
fn url_to_path(url: &str) -> &str {
    // Strip "http://" or "https://"
    let rest = if let Some(r) = url.strip_prefix("https://") {
        r
    } else if let Some(r) = url.strip_prefix("http://") {
        r
    } else {
        return url;
    };
    // remainder is "host[:port]/path" – find the first '/'
    match rest.find('/') {
        Some(i) => &rest[i..],
        None => "/",
    }
}

/// Build a `422 Unprocessable Entity` response for a filename validation failure.
///
/// The XML body mirrors what PHP/SabreDAV emits for
/// `OCA\DAV\Connector\Sabre\Exception\InvalidPath` (HTTP 422).
fn build_filename_error_response(e: FilenameError) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n  \
         <s:exception>OCA\\DAV\\Connector\\Sabre\\Exception\\InvalidPath</s:exception>\n  \
         <s:message>{e}</s:message>\n\
         </d:error>\n"
    );
    http::Response::builder()
        .status(422)
        .header(H_CSP.clone(), HeaderValue::from_static("default-src 'none';"))
        .header(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        )
        .body(Body::from(body))
        .unwrap()
}

// ─── §4.6 / §4.1 Path helpers ─────────────────────────────────────────────────

/// RFC 5987-compatible percent-encoding for use in `Content-Disposition` filename
/// parameters.
///
/// Encodes every byte that is not an RFC 3986 unreserved character
/// (`A-Z a-z 0-9 - _ . ~`).  This matches the output of PHP's `rawurlencode()`
/// which Nextcloud's `FilesPlugin::httpGet()` uses for both the `filename*` and
/// the ASCII `filename` fallback parameters.
fn percent_encode_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len() * 3);
    for &byte in name.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Choose the prefix to strip from the request path before passing to the DAV
/// filesystem.  The filesystem then sees a clean `/`-rooted path.
///
/// # Path mapping
///
/// | Incoming URL prefix                     | Strip prefix               | DAV-fs sees |
/// |-----------------------------------------|----------------------------|-------------|
/// | `/remote.php/webdav`                    | `/remote.php/webdav`       | `/{path}`   |
/// | `/remote.php/dav/files/{uid}`           | `/remote.php/dav/files/{uid}` | `/{path}` |
/// | `/remote.php/dav` (uploads, principals…)| `/remote.php/dav`          | `/{sub}`    |
/// | `/dav/files/{uid}`                      | `/dav/files/{uid}`         | `/{path}`   |
/// | `/dav`                                  | `/dav`                     | `/{sub}`    |
///
/// The `/remote.php/dav/files/{uid}` case **must** be checked before the
/// generic `/remote.php/dav` case; otherwise the `/files/{uid}` segment is
/// left in the DAV path and `dav_to_fc_path` would produce a double `files/`
/// prefix (e.g. `files/files/{uid}/Photos/img.jpg`).  (REQ §4.6)
fn determine_prefix(path: &str, uid: &str) -> String {
    if path.starts_with("/remote.php/webdav") {
        "/remote.php/webdav".to_string()
    } else if path.starts_with(&format!("/remote.php/dav/files/{uid}")) {
        // More-specific check: strip all the way through the user segment so
        // the DAV filesystem receives only the path relative to the files root.
        format!("/remote.php/dav/files/{uid}")
    } else if path.starts_with("/remote.php/dav") {
        // Other /remote.php/dav sub-trees (uploads, principals, …).
        "/remote.php/dav".to_string()
    } else if path.starts_with(&format!("/dav/files/{uid}")) {
        format!("/dav/files/{uid}")
    } else if path.starts_with("/dav") {
        "/dav".to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        determine_prefix, percent_decode_path, percent_encode_filename, url_to_path,
        write_target_name,
    };
    use http::Method;

    // ── determine_prefix ─────────────────────────────────────────────────────

    #[test]
    fn prefix_remote_webdav() {
        assert_eq!(
            determine_prefix("/remote.php/webdav/Photos/img.jpg", "alice"),
            "/remote.php/webdav"
        );
    }

    #[test]
    fn prefix_remote_webdav_root() {
        assert_eq!(
            determine_prefix("/remote.php/webdav", "alice"),
            "/remote.php/webdav"
        );
    }

    /// `/remote.php/dav/files/{uid}/...` must strip the full user prefix so
    /// that `dav_to_fc_path` does not produce a double `files/` component.
    /// This is the REQ §4.6 regression.
    #[test]
    fn prefix_remote_dav_files_uid() {
        let prefix = determine_prefix("/remote.php/dav/files/alice/Photos/img.jpg", "alice");
        assert_eq!(prefix, "/remote.php/dav/files/alice");
        // Verify the remaining DAV path maps to the right filecache path.
        let dav_after_strip = "/remote.php/dav/files/alice/Photos/img.jpg"
            .strip_prefix(&prefix)
            .unwrap();
        let fc = super::super::row::dav_to_fc_path(dav_after_strip);
        assert_eq!(fc, "files/Photos/img.jpg");
    }

    #[test]
    fn prefix_remote_dav_files_uid_root() {
        // Root collection: strip leaves an empty string → dav_to_fc_path → "files"
        let prefix = determine_prefix("/remote.php/dav/files/alice", "alice");
        assert_eq!(prefix, "/remote.php/dav/files/alice");
        let dav_after_strip = "/remote.php/dav/files/alice"
            .strip_prefix(&prefix)
            .unwrap();
        let fc = super::super::row::dav_to_fc_path(dav_after_strip);
        assert_eq!(fc, "files");
    }

    #[test]
    fn prefix_remote_dav_other() {
        // Non-files sub-trees (uploads, principals) fall through to generic /remote.php/dav.
        assert_eq!(
            determine_prefix("/remote.php/dav/uploads/alice/up123", "alice"),
            "/remote.php/dav"
        );
    }

    #[test]
    fn prefix_dav_files_uid() {
        assert_eq!(
            determine_prefix("/dav/files/alice/Documents/doc.pdf", "alice"),
            "/dav/files/alice"
        );
    }

    #[test]
    fn prefix_dav_other() {
        assert_eq!(
            determine_prefix("/dav/principals/users/alice", "alice"),
            "/dav"
        );
    }

    // ── percent_encode_filename ───────────────────────────────────────────────

    #[test]
    fn plain_ascii_unchanged() {
        assert_eq!(percent_encode_filename("hello.txt"), "hello.txt");
    }

    #[test]
    fn space_encoded() {
        assert_eq!(percent_encode_filename("my file.mp4"), "my%20file.mp4");
    }

    #[test]
    fn unicode_encoded() {
        // "café.txt" → UTF-8 bytes for 'é' are 0xC3 0xA9
        assert_eq!(percent_encode_filename("café.txt"), "caf%C3%A9.txt");
    }

    #[test]
    fn percent_sign_itself_encoded() {
        assert_eq!(percent_encode_filename("100%.csv"), "100%25.csv");
    }

    #[test]
    fn unreserved_chars_unchanged() {
        assert_eq!(
            percent_encode_filename("file-name_v1.2~beta"),
            "file-name_v1.2~beta"
        );
    }

    // ── §5.1 write_target_name ────────────────────────────────────────────────

    #[test]
    fn write_target_name_put() {
        let name = write_target_name(
            &Method::PUT,
            "/remote.php/webdav/Photos/img.jpg",
            "/remote.php/webdav",
            None,
        );
        assert_eq!(name.as_deref(), Some("img.jpg"));
    }

    #[test]
    fn write_target_name_mkcol() {
        let name = write_target_name(
            &Method::from_bytes(b"MKCOL").unwrap(),
            "/dav/files/alice/NewFolder",
            "/dav/files/alice",
            None,
        );
        assert_eq!(name.as_deref(), Some("NewFolder"));
    }

    #[test]
    fn write_target_name_move_full_url() {
        let name = write_target_name(
            &Method::from_bytes(b"MOVE").unwrap(),
            "/dav/files/alice/source.txt",
            "/dav/files/alice",
            Some("http://localhost:7000/dav/files/alice/target.txt"),
        );
        assert_eq!(name.as_deref(), Some("target.txt"));
    }

    #[test]
    fn write_target_name_copy_full_url_https() {
        let name = write_target_name(
            &Method::from_bytes(b"COPY").unwrap(),
            "/dav/files/alice/source.pdf",
            "/dav/files/alice",
            Some("https://cloud.example.com/dav/files/alice/copy.pdf"),
        );
        assert_eq!(name.as_deref(), Some("copy.pdf"));
    }

    #[test]
    fn write_target_name_get_is_none() {
        let name = write_target_name(
            &Method::GET,
            "/dav/files/alice/file.txt",
            "/dav/files/alice",
            None,
        );
        assert!(name.is_none());
    }

    #[test]
    fn percent_decode_path_space() {
        assert_eq!(percent_decode_path("my%20file.txt"), "my file.txt");
    }

    #[test]
    fn percent_decode_path_unicode() {
        // "é" is UTF-8 bytes 0xC3 0xA9
        assert_eq!(percent_decode_path("caf%C3%A9.txt"), "café.txt");
    }

    #[test]
    fn url_to_path_full_url() {
        assert_eq!(
            url_to_path("http://localhost:7000/remote.php/webdav/file.txt"),
            "/remote.php/webdav/file.txt"
        );
    }

    #[test]
    fn url_to_path_relative() {
        assert_eq!(
            url_to_path("/dav/files/alice/file.txt"),
            "/dav/files/alice/file.txt"
        );
    }

    // ── DELETE intercept path resolution ───────────────────────────────────
    //
    // The handler intercepts DELETE for directories to avoid dav-server-rs's
    // recursive child walk.  It must resolve the request path to a filecache
    // path, which requires prefix-stripping and percent-decoding.

    fn delete_intercept_fc_path(req_path: &str, strip_prefix: &str) -> String {
        let dav_path = req_path.strip_prefix(strip_prefix).unwrap_or(req_path);
        let decoded = percent_decode_path(dav_path.trim_end_matches('/'));
        crate::row::dav_to_fc_path(&decoded)
    }

    #[test]
    fn delete_intercept_percent_encoded_space() {
        // Regression: %20 must be decoded before filecache lookup.
        // Without decoding, the path "files/Media/Decent%20photos" would
        // not match the stored path "files/Media/Decent photos".
        let fc = delete_intercept_fc_path(
            "/remote.php/dav/files/admin/Media/Decent%20photos",
            "/remote.php/dav/files/admin",
        );
        assert_eq!(fc, "files/Media/Decent photos");
    }

    #[test]
    fn delete_intercept_simple_path() {
        let fc = delete_intercept_fc_path(
            "/remote.php/dav/files/admin/Photos",
            "/remote.php/dav/files/admin",
        );
        assert_eq!(fc, "files/Photos");
    }

    #[test]
    fn delete_intercept_trailing_slash() {
        // DAV paths often have a trailing slash for collections.
        let fc = delete_intercept_fc_path(
            "/remote.php/dav/files/admin/Photos/",
            "/remote.php/dav/files/admin",
        );
        assert_eq!(fc, "files/Photos");
    }

    #[test]
    fn delete_intercept_root_level_file() {
        let fc = delete_intercept_fc_path(
            "/remote.php/webdav/Notes",
            "/remote.php/webdav",
        );
        assert_eq!(fc, "files/Notes");
    }

    #[test]
    fn delete_intercept_unicode_percent_encoded() {
        // café → %C3%A9
        let fc = delete_intercept_fc_path(
            "/remote.php/dav/files/admin/caf%C3%A9",
            "/remote.php/dav/files/admin",
        );
        assert_eq!(fc, "files/café");
    }
}
