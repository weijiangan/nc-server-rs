//! Handler for bulk upload endpoint (`POST /dav/bulk`).
//!
//! Parses `multipart/related` body; per-part headers `X-File-Path`,
//! `X-OC-MTime` / `X-File-MTime`, `Content-Length`, `X-File-MD5`,
//! `OC-Checksum`.
//!
//! Response: JSON map of path → `{error, etag, fileid, permissions}`.
//!
//! Matches PHP behavior (`BulkUploadPlugin.php` / `MultipartRequestParser.php`):
//! - Per-part `Content-Length` is required; missing → parse error / 400.
//! - Per-part hash (`X-File-MD5` / `OC-Checksum`) is required and validated
//!   against the actual part content; mismatch → parse error / 400.
//! - On parse error mid-stream: returns 400 with partial results written so far.
//! - On per-file write error: records error for that file, continues processing.

use axum::{body::Body, extract::State, response::Response};
use http::{HeaderName, HeaderValue, StatusCode};
use nc_auth::AuthInfo;
use tokio::fs;
use tracing::warn;

use crate::{propagator::Propagator, row, versions, NcDavState};

static H_CSP: HeaderName = HeaderName::from_static("content-security-policy");
static H_JSON: HeaderName = HeaderName::from_static("content-type");
static JSON_VALUE: HeaderValue = HeaderValue::from_static("application/json; charset=utf-8");

/// Maximum bulk upload body size: 100 MiB.
const MAX_BULK_BODY: usize = 100 * 1024 * 1024;

/// Handler for bulk upload endpoint.
pub async fn bulk_handler(
    State(state): State<NcDavState>,
    req: axum::extract::Request,
) -> Response {
    // Extract authenticated user and instance ID from state.
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

    let instance_id = (*state.instance_id).clone();

    let content_type = match req.headers().get("content-type") {
        Some(ct) => match ct.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return bad_request_response("Invalid Content-Type"),
        },
        None => return bad_request_response("Content-Type header required"),
    };

    if !content_type.starts_with("multipart/") {
        return bad_request_response("Content-Type must be multipart");
    }

    let boundary = match extract_boundary(&content_type) {
        Some(b) => b,
        None => return bad_request_response("Invalid multipart boundary"),
    };

    let body = req.into_body();
    let bytes = match axum::body::to_bytes(body, MAX_BULK_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return http::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(
                    H_CSP.clone(),
                    HeaderValue::from_static("default-src 'none';"),
                )
                .header(H_JSON.clone(), JSON_VALUE.clone())
                .body(Body::from(format!("Failed to read body: {}", e)))
                .unwrap();
        }
    };

    let data_dir_str = state.data_directory.to_str().unwrap_or("").to_string();

    let storage_id =
        match row::lookup_storage_id(&state.pool, &state.table_prefix, &uid, &data_dir_str)
            .await
        {
            Some(id) => id,
            None => {
                return http::Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(
                        H_CSP.clone(),
                        HeaderValue::from_static("default-src 'none';"),
                    )
                    .header(H_JSON.clone(), JSON_VALUE.clone())
                    .body(Body::from("Failed to look up storage"))
                    .unwrap();
            }
        };

    let mut results: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let mut parse_error = false;

    let parser = MultipartParser::new(&bytes, &boundary);
    for part_result in parser {
        let part = match part_result {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "Failed to parse multipart part");
                parse_error = true;
                break;
            }
        };

        let file_path = match part.headers.get("x-file-path") {
            Some(p) => p.clone(),
            None => {
                results.insert(
                    format!("unknown_{}", results.len()),
                    serde_json::json!({
                        "error": true,
                        "message": "Missing X-File-Path header"
                    }),
                );
                continue;
            }
        };

        let mtime = match crate::mtime::sanitize_mtime(
            part.headers
                .get("x-oc-mtime")
                .or_else(|| part.headers.get("x-file-mtime"))
                .map(|s| s.as_str()),
        ) {
            Ok(v) => v,
            Err(msg) => {
                results.insert(
                    file_path.clone(),
                    serde_json::json!({
                        "error": true,
                        "message": msg,
                    }),
                );
                continue;
            }
        };

        // ── §10.4 Per-part Content-Length and hash validation ─────────────────
        // Matches PHP MultipartRequestParser::parseNextPart() →
        // readPartHeaders() → validateHash().
        let content_length: usize = match part.headers.get("content-length") {
            Some(cl) => match cl.parse::<usize>() {
                Ok(len) => len,
                Err(_) => {
                    results.insert(
                        file_path.clone(),
                        serde_json::json!({
                            "error": true,
                            "message": "Invalid Content-Length header",
                        }),
                    );
                    parse_error = true;
                    break;
                }
            },
            None => {
                results.insert(
                    file_path.clone(),
                    serde_json::json!({
                        "error": true,
                        "message": "The Content-Length header must not be null.",
                    }),
                );
                parse_error = true;
                break;
            }
        };

        if let Err(msg) = validate_part_hash(
            &part.data,
            content_length,
            part.headers.get("x-file-md5").map(|s| s.as_str()),
            part.headers.get("oc-checksum").map(|s| s.as_str()),
        ) {
            results.insert(
                file_path.clone(),
                serde_json::json!({
                    "error": true,
                    "message": msg,
                }),
            );
            parse_error = true;
            break;
        }

        match write_file(
            &state, &uid, &instance_id, storage_id, &file_path, &part.data, mtime,
        )
        .await
        {
            Ok(file_info) => {
                results.insert(
                    file_path.clone(),
                    serde_json::json!({
                        "error": false,
                        "etag": file_info.etag,
                        "fileid": file_info.fileid,
                        "permissions": file_info.permissions,
                    }),
                );
            }
            Err(e) => {
                tracing::error!(error = %e, path = %file_path, "Failed to write file");
                results.insert(
                    file_path.clone(),
                    serde_json::json!({
                        "error": true,
                        "message": e,
                    }),
                );
            }
        }
    }

    let status = if parse_error {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };

    let body = match serde_json::to_string(&results) {
        Ok(b) => b,
        Err(e) => {
            return http::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(
                    H_CSP.clone(),
                    HeaderValue::from_static("default-src 'none';"),
                )
                .header(H_JSON.clone(), JSON_VALUE.clone())
                .body(Body::from(format!("Failed to serialize response: {}", e)))
                .unwrap();
        }
    };

    http::Response::builder()
        .status(status)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .header(H_JSON.clone(), JSON_VALUE.clone())
        .body(Body::from(body))
        .unwrap()
}

struct FileInfo {
    etag: String,
    fileid: String,
    permissions: String,
}

async fn write_file(
    state: &NcDavState,
    uid: &str,
    instance_id: &str,
    storage_id: i64,
    file_path: &str,
    data: &[u8],
    mtime: Option<i64>,
) -> Result<FileInfo, String> {
    let fc_path = format!("files/{}", file_path.trim_start_matches('/'));

    // ── §5.2 Quota enforcement ─────────────────────────────────────────────
    // PHP BulkUploadPlugin delegates to $userFolder->newFile() which goes
    // through the storage layer where quota is enforced. We check it
    // explicitly here before writing.
    if let Err(()) = crate::quota::check_quota(
        &state.pool,
        &state.table_prefix,
        &state.appconfig_cache,
        uid,
        storage_id,
        data.len() as i64,
    )
    .await
    {
        return Err("Quota exceeded: insufficient free space".to_string());
    }

    let file_name = file_path.rsplit('/').next().unwrap_or("").to_string();
    let ext = file_name.rsplit('.').next().unwrap_or("").to_lowercase();
    let mime_str = mime_guess::from_ext(&ext)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string();
    let part_str = mime_str
        .split('/')
        .next()
        .unwrap_or("application")
        .to_string();

    // §10.8: get-or-insert mimetype IDs; mimepart is the part BEFORE
    // the '/' (e.g. "image"), NOT "image/" — matching PHP's
    // getId(substr($mimetype, 0, strpos($mimetype, '/')))
    let mime_type_id =
        nc_db::mime::get_or_insert_mime_id(&state.pool, &state.table_prefix, &state.mime_cache, &mime_str).await;
    let mimepart_id =
        nc_db::mime::get_or_insert_mime_id(&state.pool, &state.table_prefix, &state.mime_cache, &part_str).await;

    let parent_path = {
        let mut parts: Vec<&str> = fc_path.split('/').collect();
        parts.pop();
        if parts.is_empty() {
            "files".to_string()
        } else {
            // parts already start with "files" because fc_path does
            parts.join("/")
        }
    };

    let parent_row = row::lookup_by_path(&state.pool, &state.table_prefix, storage_id, &parent_path)
        .await
        .ok_or_else(|| "Parent directory not found".to_string())?;

    let existing =
        row::lookup_by_path(&state.pool, &state.table_prefix, storage_id, &fc_path).await;

    let final_disk_path = row::disk_path(&state.data_directory, uid, &fc_path);

    if let Some(parent) = final_disk_path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directory: {}", e))?;
    }

    // §9.4: save a version BEFORE overwriting the existing file.
    // Unlike davfile.rs (which writes to a temp file first), the bulk
    // handler writes directly to the final path — so we must copy the
    // old content before `fs::write()` destroys it.
    if let Some(ref old) = existing {
        let old_perms = old.permissions;
        let ext = row::get_extended(&state.pool, &state.table_prefix, old.fileid).await;
        versions::store_version(
            &state.pool,
            &state.table_prefix,
            &state.data_directory,
            uid,
            storage_id,
            &state.mime_cache,
            &fc_path,
            &final_disk_path,
            old.size,
            old.mtime,
            old.mimetype,
            old_perms,
            ext.creation_time,
            ext.upload_time,
            old.fileid,
        )
        .await;
    }

    fs::write(&final_disk_path, data)
        .await
        .map_err(|e| format!("Failed to write file: {}", e))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let file_mtime = mtime.unwrap_or(now);

    let t = filetime::FileTime::from_unix_time(file_mtime, 0);
    if let Err(e) = filetime::set_file_times(&final_disk_path, t, t) {
        warn!(path = %final_disk_path.display(), error = %e, "Failed to set file mtime on bulk upload");
    }

    let etag_raw = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
    // PHP getETag() returns quoted etag; JSON-encode doubles the quotes.
    let etag = format!("\"{}\"", etag_raw);

    let hash = row::path_hash(&fc_path);
    let fid: i64;
    if let Some(ref existing) = existing {
        fid = existing.fileid;
        let sql = format!(
            "UPDATE {prefix}filecache SET size=$1, mtime=$2, storage_mtime=$3, etag=$4, mimetype=$5, mimepart=$6 WHERE fileid=$7",
            prefix = state.table_prefix
        );
        if let Err(e) = sqlx::query(&sql)
            .bind(data.len() as i64)
            .bind(file_mtime)
            .bind(file_mtime)
            .bind(&etag_raw)
            .bind(mime_type_id)
            .bind(mimepart_id)
            .bind(fid)
            .execute(&state.pool)
            .await
        {
            warn!(fileid = fid, error = %e, "Bulk upload: failed to update filecache row");
        }
    } else {
        let sql = format!(
            "INSERT INTO {prefix}filecache \
            (storage, path, path_hash, parent, name, mimetype, mimepart, \
             size, mtime, storage_mtime, etag, permissions, checksum) \
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
            RETURNING fileid",
            prefix = state.table_prefix
        );
        fid = sqlx::query_scalar(&sql)
            .bind(storage_id)
            .bind(&fc_path)
            .bind(&hash)
            .bind(parent_row.fileid)
            .bind(&file_name)
            .bind(mime_type_id)
            .bind(mimepart_id)
            .bind(data.len() as i64)
            .bind(file_mtime)
            .bind(file_mtime)
            .bind(&etag_raw)
            .bind(27i32) // CRUDS permissions (READ|UPDATE|DELETE|SHARE)
            .bind("")
            .fetch_one(&state.pool)
            .await
            .map_err(|e| format!("Failed to insert filecache: {}", e))?;
    }

    // Set upload_time in extended cache for new files
    {
        let sql = format!(
            "INSERT INTO {prefix}filecache_extended (fileid, metadata_etag, creation_time, upload_time) \
            VALUES ($1, $2, $3, $4) \
            ON CONFLICT(fileid) DO UPDATE SET \
                upload_time = COALESCE(EXCLUDED.upload_time, {prefix}filecache_extended.upload_time)",
            prefix = state.table_prefix
        );
        if let Err(e) = sqlx::query(&sql)
            .bind(fid)
            .bind("") // metadata_etag
            .bind(file_mtime) // creation_time
            .bind(now) // upload_time
            .execute(&state.pool)
            .await
        {
            warn!(fileid = fid, error = %e, "Bulk upload: failed to upsert filecache_extended");
        }
    }

    // §9.2: propagate size/etag/mtime to the parent chain.
    {
        let old_size = existing.as_ref().map(|r| r.size).unwrap_or(0);
        let size_diff = (data.len() as i64) - old_size;
        let propagator = Propagator::new(
            state.pool.clone(),
            state.table_prefix.clone(),
            storage_id,
        );
        let _ = propagator
            .propagate_change(&fc_path, file_mtime, size_diff)
            .await;
    }

    // §9.4: insert oc_files_versions for every successful write so the
    // current mtime entry has nc:version-author.
    crate::versions::insert_version_entity(
        &state.pool,
        &state.table_prefix,
        fid,
        file_mtime,
        data.len() as i64,
        mime_type_id,
        uid,
    )
    .await;

    // fileid: PHP DavUtil::getDavFileId() formats as zero-padded 8-char id + instanceId.
    let dav_file_id = format!("{:08}{}", fid, instance_id);

    // permissions: for a newly created file owned by the user, perms=27
    // (READ|UPDATE|DELETE|SHARE), file, not shared, not mounted, renamable.
    // encode_permissions() now matches PHP DavUtil::getDavPermissions().
    let permissions = crate::props::encode_permissions(27, false, false, false, true);

    Ok(FileInfo {
        etag,
        fileid: dav_file_id,
        permissions,
    })
}

struct MultipartPart {
    headers: std::collections::HashMap<String, String>,
    data: Vec<u8>,
}

fn extract_boundary(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|param| {
        let param = param.trim();
        if param.starts_with("boundary=") {
            let value = param.trim_start_matches("boundary=");
            let value = value.trim_matches('"');
            Some(value.to_string())
        } else {
            None
        }
    })
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Streaming multipart parser that yields parts one at a time.
///
/// Mirrors PHP's `MultipartRequestParser::parseNextPart()` — returns an error
/// mid-stream if a part cannot be parsed, allowing the caller to return 400
/// with partial results (matching `BulkUploadPlugin.php:54-58`).
struct MultipartParser<'a> {
    data: &'a [u8],
    boundary_start: Vec<u8>,
    boundary_crlf: Vec<u8>,
    boundary_close: Vec<u8>,
    pos: usize,
    is_first: bool,
    done: bool,
}

impl<'a> MultipartParser<'a> {
    fn new(data: &'a [u8], boundary: &str) -> Self {
        Self {
            data,
            boundary_start: format!("--{}", boundary).into_bytes(),
            boundary_crlf: format!("\r\n--{}", boundary).into_bytes(),
            boundary_close: format!("\r\n--{}--", boundary).into_bytes(),
            pos: 0,
            is_first: true,
            done: false,
        }
    }
}

impl Iterator for MultipartParser<'_> {
    type Item = Result<MultipartPart, String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.pos >= self.data.len() {
            return None;
        }

        let start_marker = if self.is_first {
            self.is_first = false;
            &self.boundary_start[..]
        } else {
            &self.boundary_crlf[..]
        };

        if let Some(start) = find_subsequence(&self.data[self.pos..], start_marker) {
            self.pos += start + start_marker.len();
        } else {
            self.done = true;
            return None;
        }

        // Check for closing boundary (--)
        if self.pos + 2 <= self.data.len() && &self.data[self.pos..self.pos + 2] == b"--" {
            self.done = true;
            return None;
        }

        // Skip CRLF after boundary
        if self.pos + 2 <= self.data.len() && &self.data[self.pos..self.pos + 2] == b"\r\n" {
            self.pos += 2;
        }

        let header_end = match find_subsequence(&self.data[self.pos..], b"\r\n\r\n") {
            Some(h) => h,
            None => {
                self.done = true;
                return Some(Err("Missing header/body separator".to_string()));
            }
        };
        let header_bytes = &self.data[self.pos..self.pos + header_end];
        let header_text = match std::str::from_utf8(header_bytes) {
            Ok(t) => t,
            Err(e) => {
                self.done = true;
                return Some(Err(format!("Invalid header encoding: {}", e)));
            }
        };
        self.pos += header_end + 4;

        let mut headers = std::collections::HashMap::new();
        for line in header_text.lines() {
            if let Some(colon) = line.find(':') {
                let key = line[..colon].trim().to_lowercase();
                let value = line[colon + 1..].trim().to_string();
                headers.insert(key, value);
            }
        }

        let end_pos = find_subsequence(&self.data[self.pos..], &self.boundary_crlf)
            .or_else(|| find_subsequence(&self.data[self.pos..], &self.boundary_close))
            .unwrap_or(self.data.len() - self.pos);

        let body = self.data[self.pos..self.pos + end_pos].to_vec();
        self.pos += end_pos;

        Some(Ok(MultipartPart {
            headers,
            data: body,
        }))
    }
}

fn bad_request_response(msg: &str) -> Response {
    http::Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(
            H_CSP.clone(),
            HeaderValue::from_static("default-src 'none';"),
        )
        .header(H_JSON.clone(), JSON_VALUE.clone())
        .body(Body::from(msg.to_string()))
        .unwrap()
}

/// Compute the hex digest of `data` using the named `algorithm`.
///
/// Supported algorithms match those in [`crate::davfile::RunningHash`]:
/// `md5`, `sha1` / `sha-1`, `sha256` / `sha-256`, `sha384` / `sha-384`,
/// `sha512` / `sha-512`, `adler32` / `adler-32`.
fn compute_hash(algorithm: &str, data: &[u8]) -> Result<String, String> {
    use md5::Digest;
    match algorithm.to_lowercase().as_str() {
        "md5" => Ok(format!("{:x}", md5::Md5::digest(data))),
        "sha1" | "sha-1" => Ok(format!("{:x}", sha1::Sha1::digest(data))),
        "sha256" | "sha-256" => Ok(format!("{:x}", sha2::Sha256::digest(data))),
        "sha384" | "sha-384" => Ok(format!("{:x}", sha2::Sha384::digest(data))),
        "sha512" | "sha-512" => Ok(format!("{:x}", sha2::Sha512::digest(data))),
        "adler32" | "adler-32" => {
            let mut a = adler::Adler32::new();
            a.write_slice(data);
            Ok(format!("{:08x}", a.checksum()))
        }
        _ => Err(format!("Unknown hash algorithm: {}", algorithm)),
    }
}

/// Validate a bulk-upload part's content against its hash headers.
///
/// Mirrors PHP `MultipartRequestParser::validateHash()`:
/// 1. Verify `data.len()` matches the declared `content_length`.
/// 2. If `oc_checksum` is non-empty: parse `{ALG}:{hash}`, compute,
///    case‑sensitive compare.
/// 3. Else if `x_file_md5` is non-empty: compute MD5, case‑sensitive compare.
/// 4. Else: return `Err("No hash provided.")`.
///
/// Hash mismatch message matches PHP exactly:
/// `"Computed {algorithm} hash is incorrect ({computed})."`
fn validate_part_hash(
    data: &[u8],
    content_length: usize,
    x_file_md5: Option<&str>,
    oc_checksum: Option<&str>,
) -> Result<(), String> {
    if data.len() != content_length {
        return Err(format!(
            "Expected Content-Length {} but part body is {} bytes",
            content_length,
            data.len()
        ));
    }

    let (algorithm, expected_hash): (&str, &str) = if let Some(cs) = oc_checksum {
        if cs.is_empty() {
            if let Some(md5) = x_file_md5 {
                if md5.is_empty() {
                    return Err("No hash provided.".to_string());
                }
                ("md5", md5)
            } else {
                return Err("No hash provided.".to_string());
            }
        } else {
            match cs.split_once(':') {
                Some((alg, hash)) => (alg, hash),
                None => return Err("Invalid OC-Checksum format".to_string()),
            }
        }
    } else if let Some(md5) = x_file_md5 {
        if md5.is_empty() {
            return Err("No hash provided.".to_string());
        }
        ("md5", md5)
    } else {
        return Err("The hash headers must not be null.".to_string());
    };

    let computed = compute_hash(algorithm, data)?;

    // PHP does case‑sensitive comparison (`!==`); match it exactly.
    if expected_hash != computed {
        return Err(format!(
            "Computed {} hash is incorrect ({}).",
            algorithm, computed
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_boundary_simple() {
        assert_eq!(
            extract_boundary("multipart/related; boundary=abc123"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_extract_boundary_quoted() {
        assert_eq!(
            extract_boundary("multipart/related; boundary=\"abc123\""),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_extract_boundary_with_charset() {
        assert_eq!(
            extract_boundary("multipart/related; charset=utf-8; boundary=xyz"),
            Some("xyz".to_string())
        );
    }

    #[test]
    fn test_extract_boundary_missing() {
        assert_eq!(extract_boundary("multipart/related"), None);
    }

    #[test]
    fn test_multipart_parser_single_part() {
        let body =
            b"--boundary\r\nX-File-Path: /test.txt\r\n\r\nhello world\r\n--boundary--";
        let parser = MultipartParser::new(body, "boundary");
        let parts: Vec<_> = parser.collect();
        assert_eq!(parts.len(), 1);
        let part = parts[0].as_ref().unwrap();
        assert_eq!(part.headers.get("x-file-path").unwrap(), "/test.txt");
        assert_eq!(part.data, b"hello world");
    }

    #[test]
    fn test_multipart_parser_multiple_parts() {
        let body = b"--boundary\r\nX-File-Path: /a.txt\r\n\r\ncontent A\r\n--boundary\r\nX-File-Path: /b.txt\r\n\r\ncontent B\r\n--boundary--";
        let parser = MultipartParser::new(body, "boundary");
        let parts: Vec<_> = parser.collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0].as_ref().unwrap().headers.get("x-file-path").unwrap(),
            "/a.txt"
        );
        assert_eq!(parts[0].as_ref().unwrap().data, b"content A");
        assert_eq!(
            parts[1].as_ref().unwrap().headers.get("x-file-path").unwrap(),
            "/b.txt"
        );
        assert_eq!(parts[1].as_ref().unwrap().data, b"content B");
    }

    #[test]
    fn test_multipart_parser_binary_data() {
        let body: Vec<u8> = [
            b"--boundary\r\nX-File-Path: /bin.dat\r\n\r\n".to_vec(),
            vec![0x00, 0x01, 0x02, 0xff, 0xfe],
            b"\r\n--boundary--".to_vec(),
        ]
        .concat();
        let parser = MultipartParser::new(&body, "boundary");
        let parts: Vec<_> = parser.collect();
        assert_eq!(parts.len(), 1);
        let part = parts[0].as_ref().unwrap();
        assert_eq!(part.headers.get("x-file-path").unwrap(), "/bin.dat");
        assert_eq!(part.data, vec![0x00, 0x01, 0x02, 0xff, 0xfe]);
    }

    #[test]
    fn test_multipart_parser_missing_separator() {
        let body = b"--boundary\r\nX-File-Path: /test.txt";
        let parser = MultipartParser::new(body, "boundary");
        let parts: Vec<_> = parser.collect();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            Err(e) => assert!(e.contains("Missing header/body separator")),
            Ok(_) => panic!("Expected error for missing separator"),
        }
    }

    #[test]
    fn test_multipart_parser_with_mtime() {
        let body = b"--boundary\r\nX-File-Path: /test.txt\r\nX-OC-MTime: 1234567890\r\n\r\ncontent\r\n--boundary--";
        let parser = MultipartParser::new(body, "boundary");
        let parts: Vec<_> = parser.collect();
        assert_eq!(parts.len(), 1);
        let part = parts[0].as_ref().unwrap();
        assert_eq!(part.headers.get("x-oc-mtime").unwrap(), "1234567890");
    }

    #[test]
    fn test_multipart_parser_with_file_mtime() {
        let body = b"--boundary\r\nX-File-Path: /test.txt\r\nX-File-MTime: 9876543210\r\n\r\ncontent\r\n--boundary--";
        let parser = MultipartParser::new(body, "boundary");
        let parts: Vec<_> = parser.collect();
        assert_eq!(parts.len(), 1);
        let part = parts[0].as_ref().unwrap();
        assert_eq!(part.headers.get("x-file-mtime").unwrap(), "9876543210");
    }

    #[test]
    fn test_multipart_parser_empty_body() {
        let body = b"";
        let parser = MultipartParser::new(body, "boundary");
        let parts: Vec<_> = parser.collect();
        assert_eq!(parts.len(), 0);
    }

    #[test]
    fn test_multipart_parser_first_part_no_crlf_prefix() {
        let body = b"--boundary\r\nX-File-Path: /first.txt\r\n\r\nfirst\r\n--boundary\r\nX-File-Path: /second.txt\r\n\r\nsecond\r\n--boundary--";
        let parser = MultipartParser::new(body, "boundary");
        let parts: Vec<_> = parser.collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].as_ref().unwrap().data, b"first");
        assert_eq!(parts[1].as_ref().unwrap().data, b"second");
    }

    // ── fc_path construction ───────────────────────────────────────────────
    //
    // regression: double "files/" prefix when the client-provided path
    // already contained the "files/" prefix.
    //   core-rs/crates/nc-dav/src/bulk_handler.rs::store_single_file()
    //     → format!("files/{}", file_path.trim_start_matches('/'))

    /// Replicate the fc_path construction from `store_single_file`.
    fn bulk_fc_path(file_path: &str) -> String {
        format!("files/{}", file_path.trim_start_matches('/'))
    }

    #[test]
    fn bulk_fc_path_simple() {
        assert_eq!(bulk_fc_path("/test.txt"), "files/test.txt");
    }

    #[test]
    fn bulk_fc_path_nested() {
        assert_eq!(
            bulk_fc_path("/Media/Decent photos/001.jpg"),
            "files/Media/Decent photos/001.jpg"
        );
    }

    #[test]
    fn bulk_fc_path_no_leading_slash() {
        assert_eq!(bulk_fc_path("test.txt"), "files/test.txt");
    }

    #[test]
    fn bulk_fc_path_no_double_files_prefix() {
        // Regression: the old code `format!("files/{}", parts.join("/"))`
        // where parts already included "files" produced "files/files/...".
        // trim_start_matches('/') does NOT strip "files/", so a path
        // already containing "files/" would still cause a double prefix.
        // This test documents the contract: callers must NOT pass paths
        // already prefixed with "files/".
        let with_files_prefix = bulk_fc_path("files/test.txt");
        assert_eq!(
            with_files_prefix,
            "files/files/test.txt",
            "callers must strip 'files/' prefix before passing to bulk_fc_path"
        );
        // Correct: strip the DAV prefix first.
        assert_eq!(bulk_fc_path("test.txt"), "files/test.txt");
    }

    // ── §10.4 hash validation tests ─────────────────────────────────────────

    #[test]
    fn compute_hash_md5() {
        let hash = compute_hash("md5", b"hello world").unwrap();
        assert_eq!(hash, "5eb63bbbe01eeed093cb22bb8f5acdc3");
    }

    #[test]
    fn compute_hash_sha1() {
        let hash = compute_hash("sha1", b"hello world").unwrap();
        assert_eq!(hash, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");
    }

    #[test]
    fn compute_hash_sha256() {
        let hash = compute_hash("sha256", b"hello world").unwrap();
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn compute_hash_unknown_algorithm() {
        let err = compute_hash("superhash", b"data").unwrap_err();
        assert!(err.contains("Unknown hash algorithm"));
    }

    #[test]
    fn validate_part_hash_oc_checksum_ok() {
        let data = b"hello world";
        let hash = compute_hash("md5", data).unwrap();
        let header = format!("md5:{}", hash);
        assert!(validate_part_hash(data, data.len(), None, Some(&header)).is_ok());
    }

    #[test]
    fn validate_part_hash_x_file_md5_ok() {
        let data = b"hello world";
        let hash = compute_hash("md5", data).unwrap();
        assert!(validate_part_hash(data, data.len(), Some(&hash), None).is_ok());
    }

    #[test]
    fn validate_part_hash_mismatch() {
        let data = b"hello world";
        let err = validate_part_hash(data, data.len(), Some("deadbeef"), None).unwrap_err();
        assert!(err.contains("Computed md5 hash is incorrect"));
        assert!(err.contains(&compute_hash("md5", data).unwrap()));
    }

    #[test]
    fn validate_part_hash_content_length_mismatch() {
        let data = b"hello world";
        let err = validate_part_hash(data, 999, Some("ignored"), None).unwrap_err();
        assert!(err.contains("Content-Length"));
        assert!(err.contains("999"));
    }

    #[test]
    fn validate_part_hash_missing_both_headers() {
        let data = b"hello world";
        let err = validate_part_hash(data, data.len(), None, None).unwrap_err();
        assert_eq!(err, "The hash headers must not be null.");
    }

    #[test]
    fn validate_part_hash_oc_checksum_empty_falls_back_to_md5() {
        let data = b"hello world";
        let hash = compute_hash("md5", data).unwrap();
        // OC-Checksum empty, X-File-MD5 present → use MD5
        assert!(validate_part_hash(data, data.len(), Some(&hash), Some("")).is_ok());
    }

    #[test]
    fn validate_part_hash_both_empty_is_error() {
        let data = b"hello world";
        let err = validate_part_hash(data, data.len(), Some(""), Some("")).unwrap_err();
        assert_eq!(err, "No hash provided.");
    }

    #[test]
    fn validate_part_hash_oc_checksum_sha1() {
        let data = b"hello world";
        let hash = compute_hash("sha1", data).unwrap();
        let header = format!("SHA1:{}", hash);
        assert!(validate_part_hash(data, data.len(), None, Some(&header)).is_ok());
    }

    #[test]
    fn validate_part_hash_oc_checksum_sha256() {
        let data = b"hello world";
        let hash = compute_hash("sha256", data).unwrap();
        let header = format!("sha256:{}", hash);
        assert!(validate_part_hash(data, data.len(), None, Some(&header)).is_ok());
    }

    #[test]
    fn validate_part_hash_adler32() {
        let data = b"hello world";
        let hash = compute_hash("adler32", data).unwrap();
        let header = format!("adler32:{}", hash);
        assert!(validate_part_hash(data, data.len(), None, Some(&header)).is_ok());
    }

    #[test]
    fn validate_part_hash_case_sensitive_mismatch() {
        // PHP uses strict !== comparison; uppercase hash must NOT match
        // lowercase computed hash.
        let data = b"hello world";
        let hash = compute_hash("md5", data).unwrap(); // lowercase
        let upper = hash.to_uppercase();
        assert_ne!(hash, upper, "sanity: uppercased hash differs from lower");
        let err = validate_part_hash(data, data.len(), Some(&upper), None).unwrap_err();
        assert!(err.contains("Computed md5 hash is incorrect"));
    }

    #[test]
    fn validate_part_hash_empty_content_ok() {
        // Content-Length 0 → empty data, hash of empty string matches.
        let empty: &[u8] = b"";
        let hash = compute_hash("md5", empty).unwrap(); // d41d8cd98f00b204e9800998ecf8427e
        assert!(validate_part_hash(empty, 0, Some(&hash), None).is_ok());
    }
}
