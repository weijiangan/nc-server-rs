//! ZIP / TAR folder download (PHASE-5.10 / REQ §7.5).
//!
//! When a GET request targets a DAV collection and the client signals
//! archive download interest (via `Accept` header or `?accept=` query
//! parameter), the full contents or a filtered subset are streamed as a
//! ZIP or TAR archive.
//!
//! Mirrors PHP `ZipFolderPlugin::handleDownload()` in
//! `apps/dav/lib/Connector/Sabre/ZipFolderPlugin.php`.
//!
//! ## Architecture
//!
//! **Two modes** based on estimated archive size (sum of file sizes from
//! `oc_filecache`):
//! - **Buffered** (≤10 MiB): builds archive in memory (`Vec<u8>`), sets
//!   `Content-Length` header.
//! - **Streaming** (>10 MiB): writes archive incrementally via `mpsc`
//!   channel → `Body::from_stream()`, no `Content-Length` (chunked
//!   transfer encoding). Max in-flight memory ≈512 KiB.
//!
//! - **Streaming ZIP** uses `s-zip` (`StreamingZipWriter`) — sequential
//!   writes, no seeking required. Runs in `spawn_blocking`.
//! - **Streaming TAR** uses `tar::Builder` + custom `StreamingWriter`
//!   (`Write + Seek` that emits chunks to the channel).
//! - Both ZIP and TAR stream file contents from disk in 32 KiB blocks.
//! - `Content-Disposition` uses RFC 5987 encoding.

use std::io::Write;
use std::path::PathBuf;

use axum::body::Body;
use axum::response::Response;
use http::{HeaderName, HeaderValue, StatusCode};
use nc_db::appconfig::SharedAppConfigCache;
use nc_db::mime::SharedMimeCache;
use nc_db::pool::DbPool;
use tar::Builder as TarBuilder;
use zip::write::FileOptions as ZipFileOptions;
use zip::ZipWriter;

use crate::row;

static H_CSP: HeaderName = HeaderName::from_static("content-security-policy");
static H_X_ACCEL_BUFFERING: HeaderName = HeaderName::from_static("x-accel-buffering");

/// Estimated total file size (bytes) above which we use streaming instead of
/// buffering the entire archive in memory.  10 MiB — small enough that
/// buffering is cheap, large enough that most folder downloads still stream.
const STREAM_THRESHOLD: u64 = 10 * 1024 * 1024;

// ─── Public entry point ────────────────────────────────────────────────────────

/// Attempt to serve a DAV collection as a ZIP/TAR archive.
///
/// Returns `Some(response)` when the request signals archive interest and the
/// target resolves to a directory.  Returns `None` to let the caller fall
/// through to the standard handler (DummyGetResponsePlugin).
pub async fn try_serve_archive(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    fc_path: &str,
    data_dir: &std::path::Path,
    uid: &str,
    mime_cache: &SharedMimeCache,
    _appconfig_cache: &SharedAppConfigCache,
    accept_header: Option<&str>,
    uri_query: Option<&str>,
    x_nc_files: Vec<String>,
    request_id: &str,
) -> Option<Response> {
    // 1. Determine if archive format is requested.
    let format = resolve_format(accept_header, uri_query)?;
    tracing::debug!(format = ?format, fc_path, "§5.10 archive download requested");

    // 2. Confirm this is a directory.
    let fc_row = row::lookup_by_path(pool, prefix, storage_id, fc_path).await?;
    // §10.8: use get-or-insert for correctness even on reads
    let dir_mimetype_id =
        nc_db::mime::get_or_insert_mime_id(pool, prefix, mime_cache, "httpd/unix-directory").await;
    if fc_row.mimetype != dir_mimetype_id {
        return None;
    }

    // 3. Parse child-name filter (?files= or X-NC-Files headers).
    //    If filter has invalid values (PHP behaviour) we bail entirely.
    let (filter, has_invalid) = parse_files_filter(uri_query, &x_nc_files);
    if has_invalid {
        tracing::debug!(
            ?uri_query,
            "§5.10 invalid files filter, falling through to default"
        );
        return None;
    }
    let filtered = !filter.is_empty();

    // 4. Archive name (matches PHP ZipFolderPlugin).
    //    PHP checks: count(explode('/', trim(path, '/'), 3)) === 2 → root folder.
    //    For our fc_path this means "files" (single segment, no sub-path).
    let archive_name = if fc_path == "files" {
        "download".to_string()
    } else {
        fc_path.rsplit('/').next().unwrap_or("download").to_string()
    };

    // 5. Collect entries.
    let children = if filtered {
        collect_filtered_children(
            pool,
            prefix,
            storage_id,
            &filter,
            fc_path,
            data_dir,
            uid,
            dir_mimetype_id,
        )
        .await
    } else {
        collect_all_children(
            pool,
            prefix,
            storage_id,
            fc_path,
            data_dir,
            uid,
            dir_mimetype_id,
        )
        .await
    };

    // 6. Estimate total size (sum of file sizes from DB).  Directories contribute 0.
    let estimated_size: u64 = children.iter().map(|e| e.size).sum();
    let use_streaming = estimated_size > STREAM_THRESHOLD;

    // 7. Build response headers (shared between streaming and buffered).
    let ext = match format {
        ArchiveFormat::Zip => "zip",
        ArchiveFormat::Tar => "tar",
    };
    let full_name = format!("{archive_name}.{ext}");
    let encoded = percent_encode_filename(&full_name);
    let content_disposition =
        format!("attachment; filename*=UTF-8''{encoded}; filename=\"{encoded}\"");
    let content_type = match format {
        ArchiveFormat::Zip => "application/zip",
        ArchiveFormat::Tar => "application/x-tar",
    };
    let include_top_dir = !filtered;

    let mut resp = if use_streaming {
        // ── Streaming mode: no Content-Length, chunked transfer encoding ──
        tracing::debug!(
            estimated_size,
            "§5.10 streaming archive (estimated > {} MiB)",
            STREAM_THRESHOLD / (1024 * 1024)
        );
        let stream = crate::archive_stream::ArchiveStream::spawn(
            format,
            archive_name,
            children,
            include_top_dir,
        );

        http::Response::builder()
            .status(StatusCode::OK)
            .header(
                H_CSP.clone(),
                HeaderValue::from_static("default-src 'none';"),
            )
            .header(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static(content_type),
            )
            .header(H_X_ACCEL_BUFFERING.clone(), HeaderValue::from_static("no"))
            .header(
                http::header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&content_disposition).unwrap(),
            )
            // No Content-Length — chunked transfer encoding.
            .body(Body::from_stream(stream))
            .unwrap()
    } else {
        // ── Buffered mode: entire archive in memory, Content-Length set ──
        tracing::debug!(
            estimated_size,
            "§5.10 buffered archive (estimated <= {} MiB)",
            STREAM_THRESHOLD / (1024 * 1024)
        );
        let buf = build_archive_buffered(format, &archive_name, &children, include_top_dir);

        http::Response::builder()
            .status(StatusCode::OK)
            .header(
                H_CSP.clone(),
                HeaderValue::from_static("default-src 'none';"),
            )
            .header(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static(content_type),
            )
            .header(H_X_ACCEL_BUFFERING.clone(), HeaderValue::from_static("no"))
            .header(
                http::header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&content_disposition).unwrap(),
            )
            .header(
                http::header::CONTENT_LENGTH,
                HeaderValue::from_str(&buf.len().to_string()).unwrap(),
            )
            .body(Body::from(buf))
            .unwrap()
    };

    if let Ok(v) = HeaderValue::from_str(request_id) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-request-id"), v);
    }
    if let Ok(v) = HeaderValue::from_str(uid) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-user-id"), v);
    }

    Some(resp)
}

// ─── Format resolution ─────────────────────────────────────────────────────────

/// Archive format — also used by the streaming module (`archive_stream`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum ArchiveFormat {
    Zip,
    Tar,
}

/// Check Accept header and/or `?accept=` query parameter.
///
/// When `?accept=` is present it **overwrites** the Accept header (PHP behavior).
/// Returns `None` when neither indicates archive interest → fall through to default.
fn resolve_format(accept_header: Option<&str>, query: Option<&str>) -> Option<ArchiveFormat> {
    // Query param takes precedence / overwrites.
    if let Some(q) = query {
        for param in q.split('&') {
            if let Some(val) = param.strip_prefix("accept=") {
                return match val {
                    "zip" | "application/zip" => Some(ArchiveFormat::Zip),
                    "tar" | "application/x-tar" => Some(ArchiveFormat::Tar),
                    _ => continue,
                };
            }
        }
    }

    let h = accept_header?.to_lowercase();
    if h.contains("zip") || h.contains("application/zip") {
        return Some(ArchiveFormat::Zip);
    }
    if h.contains("tar") || h.contains("application/x-tar") {
        return Some(ArchiveFormat::Tar);
    }

    None
}

// ─── Filter parsing ────────────────────────────────────────────────────────────

/// Parse `?files=["a","b"]` (URL-encoded JSON array) or `X-NC-Files` headers.
///
/// Returns `(names, has_invalid)`.  `has_invalid` is set when the JSON parses
/// but contains a non-string element — PHP logs a notice **and falls back to
/// the default SabreDAV behaviour** (i.e. no archive).
fn parse_files_filter(query: Option<&str>, headers: &[String]) -> (Vec<String>, bool) {
    if let Some(q) = query {
        for param in q.split('&') {
            if let Some(val) = param.strip_prefix("files=") {
                let decoded = url_decode(val);
                if let Ok(arr) = serde_json::from_str::<serde_json::Value>(&decoded) {
                    if let Some(a) = arr.as_array() {
                        let mut names = Vec::new();
                        for v in a {
                            if let Some(s) = v.as_str() {
                                names.push(s.to_string());
                            } else {
                                return (Vec::new(), true); // invalid element
                            }
                        }
                        return (names, false);
                    }
                    // Single value (non-array) — PHP wraps in an array.
                    if let Some(s) = arr.as_str() {
                        return (vec![s.to_string()], false);
                    }
                    return (Vec::new(), true);
                }
                break;
            }
        }
    }

    (headers.to_vec(), false)
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_nibble(b[i + 1]), hex_nibble(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
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

// ─── Child collection ──────────────────────────────────────────────────────────

/// A file or directory entry to include in the archive.
/// Also used by the streaming module (`archive_stream`).
pub(crate) struct ArchiveEntry {
    /// Path inside the archive (relative to the archive root / folder).
    pub(crate) archive_path: String,
    /// Absolute OS path.  Empty for directory entries that need no content.
    pub(crate) disk_path: PathBuf,
    /// True = directory entry.
    pub(crate) is_dir: bool,
    /// mtime as Unix timestamp.
    pub(crate) mtime: u64,
    /// File size (0 for directories).
    pub(crate) size: u64,
}

/// Collect every descendant of `fc_path` for a **full** (unfiltered) download.
///
/// PHP behaviour (PHP `ZipFolderPlugin::handleDownload`):
///   `$rootPath = dirname($folder->getPath())` — strip parent so that the
///   folder **itself** and all its descendants appear relative to it inside
///   the archive.  The top-level folder is then added as an empty directory
///   before streaming children.
async fn collect_all_children(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    fc_path: &str,
    data_dir: &std::path::Path,
    uid: &str,
    dir_mimetype_id: i64,
) -> Vec<ArchiveEntry> {
    let root_path = parent_fc_path(fc_path);

    let Some(dir_row) = row::lookup_by_path(pool, prefix, storage_id, fc_path).await else {
        return Vec::new();
    };

    collect_node(
        pool,
        prefix,
        storage_id,
        &dir_row,
        &root_path,
        data_dir,
        uid,
        dir_mimetype_id,
    )
    .await
}

/// Collect only the explicitly named children for a **filtered** download.
///
/// PHP behaviour:
///   `$rootPath = $folder->getPath()` — strip the folder's own path so that
///   listed children appear at the archive root (no enclosing directory).
///   Only the listed children — not siblings — are included, recursively.
async fn collect_filtered_children(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    child_names: &[String],
    fc_path: &str,
    data_dir: &std::path::Path,
    uid: &str,
    dir_mimetype_id: i64,
) -> Vec<ArchiveEntry> {
    let root_path = fc_path.to_string();

    let mut entries = Vec::new();
    for name in child_names {
        let child_fc = format!("{fc_path}/{name}");
        if let Some(child_row) = row::lookup_by_path(pool, prefix, storage_id, &child_fc).await {
            let sub = collect_node(
                pool,
                prefix,
                storage_id,
                &child_row,
                &root_path,
                data_dir,
                uid,
                dir_mimetype_id,
            )
            .await;
            entries.extend(sub);
        }
    }
    entries
}

/// Recursively resolve entries under `row`, producing archive paths
/// relative to `root_path`.
async fn collect_node(
    pool: &DbPool,
    prefix: &str,
    storage_id: i64,
    row: &row::FileCacheRow,
    root_path: &str,
    data_dir: &std::path::Path,
    uid: &str,
    dir_mimetype_id: i64,
) -> Vec<ArchiveEntry> {
    let is_dir = row.mimetype == dir_mimetype_id;

    let fc_path = row.path.as_deref().unwrap_or("");
    let archive_path = if root_path.is_empty() {
        fc_path.to_string()
    } else {
        fc_path
            .strip_prefix(&format!("{root_path}/"))
            .unwrap_or(fc_path.strip_prefix(root_path).unwrap_or(fc_path))
            .to_string()
    };

    // Skip the root entry itself when its archive path equals the original
    // root_path (full download, root dir).
    if is_dir && archive_path.is_empty() {
        let children = row::list_children(pool, prefix, row.fileid, storage_id).await;
        let mut all = Vec::new();
        for child in &children {
            all.extend(
                Box::pin(collect_node(
                    pool,
                    prefix,
                    storage_id,
                    child,
                    root_path,
                    data_dir,
                    uid,
                    dir_mimetype_id,
                ))
                .await,
            );
        }
        return all;
    }

    let disk = crate::row::disk_path(data_dir, uid, fc_path);

    if !is_dir {
        return vec![ArchiveEntry {
            archive_path,
            disk_path: disk,
            is_dir: false,
            mtime: row.mtime.max(0) as u64,
            size: row.size.max(0) as u64,
        }];
    }

    // Directory entry + recurse into children.
    let mut result = vec![ArchiveEntry {
        archive_path,
        disk_path: PathBuf::new(),
        is_dir: true,
        mtime: row.mtime.max(0) as u64,
        size: 0,
    }];

    let children = row::list_children(pool, prefix, row.fileid, storage_id).await;
    for child in &children {
        result.extend(
            Box::pin(collect_node(
                pool,
                prefix,
                storage_id,
                child,
                root_path,
                data_dir,
                uid,
                dir_mimetype_id,
            ))
            .await,
        );
    }

    result
}

/// Parent fc_path — e.g. "files/Photos" → "files", "files" → "".
fn parent_fc_path(fc_path: &str) -> String {
    fc_path
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

// ─── Archive building (buffered, fallback for small archives) ──────────────────

/// Build the entire archive into a single in-memory buffer.
///
/// Used as a fallback for small archives where the total estimated size
/// is below `STREAM_THRESHOLD`.  Allows setting a `Content-Length` header.
fn build_archive_buffered(
    format: ArchiveFormat,
    archive_name: &str,
    entries: &[ArchiveEntry],
    include_top_dir: bool,
) -> Vec<u8> {
    let mut buf = Vec::new();
    match format {
        ArchiveFormat::Zip => {
            let mut zip = ZipWriter::new(std::io::Cursor::new(&mut buf));

            if include_top_dir {
                let mtime = unix_to_zip_datetime(entries.first().map(|e| e.mtime).unwrap_or(0));
                let opts = ZipFileOptions::<()>::default().last_modified_time(mtime);
                let _ = zip.add_directory(archive_name, opts);
            }

            for entry in entries {
                if entry.archive_path.is_empty() {
                    continue;
                }
                if entry.is_dir {
                    let mtime = unix_to_zip_datetime(entry.mtime);
                    let opts = ZipFileOptions::<()>::default().last_modified_time(mtime);
                    let _ = zip.add_directory(&entry.archive_path, opts);
                } else {
                    let mtime = unix_to_zip_datetime(entry.mtime);
                    let opts = ZipFileOptions::<()>::default().last_modified_time(mtime);
                    if zip.start_file(&entry.archive_path, opts).is_err() {
                        tracing::error!(path = %entry.archive_path, "zip start_file failed");
                        continue;
                    }
                    if let Ok(data) = std::fs::read(&entry.disk_path) {
                        let _ = zip.write_all(&data);
                    } else {
                        tracing::warn!(
                            path = %entry.disk_path.display(),
                            "failed to read file for archive"
                        );
                    }
                }
            }

            let _ = zip.finish();
        }
        ArchiveFormat::Tar => {
            let mut tar = TarBuilder::new(Vec::new());

            for entry in entries {
                if entry.archive_path.is_empty() {
                    continue;
                }
                if entry.is_dir {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(0);
                    header.set_mtime(entry.mtime);
                    header.set_mode(0o755);
                    header.set_entry_type(tar::EntryType::Directory);
                    let path = format!("{}/", entry.archive_path);
                    header.set_cksum();
                    let _ = tar.append_data(&mut header, &path, std::io::empty());
                } else if let Ok(file) = std::fs::File::open(&entry.disk_path) {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(entry.size);
                    header.set_mtime(entry.mtime);
                    header.set_mode(0o644);
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_cksum();
                    let _ = tar.append_data(&mut header, &entry.archive_path, file);
                } else {
                    tracing::warn!(
                        path = %entry.disk_path.display(),
                        "failed to open file for archive"
                    );
                }
            }

            let _ = tar.finish();
            if let Ok(inner) = tar.into_inner() {
                buf = inner;
            }
        }
    }
    buf
}

/// Convert a Unix timestamp to a `zip::DateTime`, falling back to the
/// minimum valid date (1980-01-01 00:00:00) if out of range.
fn unix_to_zip_datetime(ts: u64) -> zip::DateTime {
    let total_secs = ts as i64;
    let secs_of_day = total_secs % 86400;
    let mut days = total_secs / 86400;
    let hour = (secs_of_day / 3600) as u8;
    let minute = ((secs_of_day % 3600) / 60) as u8;
    let second = (secs_of_day % 60) as u8;

    // Convert Gregorian day number to Y-M-D.
    // Algorithm from Howard Hinnant's date algorithms (chrono-compatible).
    days += 719_468; // days since 0000-03-01
    let era = if days >= 0 {
        days / 146_097
    } else {
        (days - 146_096) / 146_097
    };
    let yoe = days - era * 146_097;
    let y = (yoe - yoe / 1460 + yoe / 36_524 - yoe / 146_096) / 365;
    let year = y + era * 400;
    let doy = yoe - (365 * y + y / 4 - y / 100 + y / 400);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };

    let year = year.max(1980).min(2107) as u16;
    let month = month.clamp(1, 12) as u8;
    let day = day.min(31).max(1);

    zip::DateTime::from_date_and_time(year, month, day, hour, minute, second)
        .unwrap_or_else(|_| zip::DateTime::default())
}

/// RFC 5987-compatible encoding for `Content-Disposition` filename parameters.
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

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_format_accept_header_zip() {
        assert!(matches!(
            resolve_format(Some("application/zip"), None),
            Some(ArchiveFormat::Zip)
        ));
    }

    #[test]
    fn resolve_format_accept_header_tar() {
        assert!(matches!(
            resolve_format(Some("application/x-tar"), None),
            Some(ArchiveFormat::Tar)
        ));
    }

    #[test]
    fn resolve_format_query_param_zip() {
        assert!(matches!(
            resolve_format(None, Some("accept=zip")),
            Some(ArchiveFormat::Zip)
        ));
    }

    #[test]
    fn resolve_format_query_param_tar() {
        assert!(matches!(
            resolve_format(None, Some("accept=tar")),
            Some(ArchiveFormat::Tar)
        ));
    }

    #[test]
    fn resolve_format_query_overrides_accept() {
        assert!(matches!(
            resolve_format(Some("application/x-tar"), Some("accept=zip")),
            Some(ArchiveFormat::Zip)
        ));
    }

    #[test]
    fn resolve_format_unrelated_accept() {
        assert!(resolve_format(Some("text/html"), None).is_none());
    }

    #[test]
    fn resolve_archive_name_subfolder() {
        let name = "files/Photos".rsplit('/').next().unwrap_or("download");
        assert_eq!(name, "Photos");
    }

    #[test]
    fn resolve_archive_name_root() {
        let fc_path = "files";
        let name = if fc_path == "files" {
            "download".to_string()
        } else {
            fc_path.rsplit('/').next().unwrap_or("download").to_string()
        };
        assert_eq!(name, "download");
    }

    #[test]
    fn parse_files_from_json_array() {
        let (names, invalid) =
            parse_files_filter(Some("files=%5B%22a.txt%22%2C%22b.txt%22%5D"), &[]);
        assert!(!invalid);
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn parse_files_from_headers() {
        let (names, invalid) =
            parse_files_filter(None, &["a.txt".to_string(), "b.txt".to_string()]);
        assert!(!invalid);
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn parse_files_invalid_json_element() {
        let (names, invalid) = parse_files_filter(Some("files=%5B1%2C2%5D"), &[]);
        assert!(invalid);
        assert!(names.is_empty());
    }

    #[test]
    fn parent_fc_path_subfolder() {
        assert_eq!(parent_fc_path("files/Photos"), "files");
    }

    #[test]
    fn parent_fc_path_root() {
        assert_eq!(parent_fc_path("files"), "");
    }

    #[test]
    fn parent_fc_path_deeply_nested() {
        assert_eq!(parent_fc_path("files/a/b/c"), "files/a/b");
    }

    #[test]
    fn unix_to_zip_datetime_epoch() {
        // Epoch (1970-01-01) is before ZIP's minimum year (1980), so it's clamped.
        let dt = unix_to_zip_datetime(0);
        assert_eq!(dt.year(), 1980);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 1);
        assert_eq!(dt.hour(), 0);
        assert_eq!(dt.minute(), 0);
        assert_eq!(dt.second(), 0);
    }

    #[test]
    fn unix_to_zip_datetime_known() {
        // 2024-06-15 10:30:00 UTC = 1718447400
        let dt = unix_to_zip_datetime(1718447400);
        assert_eq!(dt.year(), 2024);
        assert_eq!(dt.month(), 6);
        assert_eq!(dt.day(), 15);
        assert_eq!(dt.hour(), 10);
        assert_eq!(dt.minute(), 30);
        assert_eq!(dt.second(), 0);
    }

    #[test]
    fn percent_encode_preserves_unreserved() {
        assert_eq!(
            percent_encode_filename("hello-world.txt"),
            "hello-world.txt"
        );
    }

    #[test]
    fn percent_encode_space() {
        assert_eq!(percent_encode_filename("my file.tar"), "my%20file.tar");
    }

    #[test]
    fn percent_encode_unicode() {
        assert_eq!(percent_encode_filename("café.zip"), "caf%C3%A9.zip");
    }
}
