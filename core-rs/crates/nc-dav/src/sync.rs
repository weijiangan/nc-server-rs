//! RFC 6578 `sync-collection` REPORT handler (PHASE-4.11).
//!
//! `dav-server-rs` does not implement RFC 6578 natively, so the Axum handler
//! intercepts all `REPORT` requests and delegates `sync-collection` ones here.
//!
//! ## Token format
//!
//! `http://sabre.io/ns/sync/{MAX(mtime)}` — identical to the format used by
//! SabreDAV / Nextcloud PHP so existing desktop sync clients work without
//! changes.
//!
//! ## Deletion semantics
//!
//! Without an audit-log table we cannot know which nodes have been deleted
//! since the previous sync.  The response therefore contains only
//! additions and modifications.  The desktop sync client performs a periodic
//! full PROPFIND to reconcile deletions; this is the same behaviour as the
//! PHP reference implementation when the `Changes` backend is not enabled.
//!
//! ## Request body
//!
//! ```xml
//! <d:sync-collection xmlns:d="DAV:">
//!   <d:sync-token>http://sabre.io/ns/sync/1705322096</d:sync-token>
//!   <d:sync-level>1</d:sync-level>
//!   <d:prop>…</d:prop>
//! </d:sync-collection>
//! ```
//!
//! ## Response
//!
//! `207 Multi-Status` per RFC 6578 §6.1 followed by a `<d:sync-token>`
//! element containing the updated token.

use std::fmt::Write as _;
use std::time::{Duration, UNIX_EPOCH};

use axum::body::Body;
use http::{Response, StatusCode};

use crate::{props, row, NcDavState};

// ─── Token constant ───────────────────────────────────────────────────────────

pub const SYNC_TOKEN_PREFIX: &str = "http://sabre.io/ns/sync/";

// ─── Request parsing ──────────────────────────────────────────────────────────

/// Result of parsing a `REPORT` body.
pub struct SyncRequest {
    /// Unix timestamp extracted from the client's `<d:sync-token>`, or `None`
    /// if the token was absent / empty (initial sync → return all nodes).
    pub since_mtime: Option<i64>,
    /// `true` when the root XML element is `{DAV:}sync-collection`.
    pub is_sync_collection: bool,
}

/// Parse a `REPORT` request body.
///
/// Returns `is_sync_collection = false` for any root element other than
/// `{DAV:}sync-collection`, allowing the caller to fall through for other
/// REPORT types (e.g. `principal-property-search`).
pub fn parse_report_body(body: &[u8]) -> SyncRequest {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    // quick-xml 0.37: `from_reader` over a BufRead uses `read_event_into`;
    // converting to a str-backed reader gives the allocation-free `read_event`.
    let body_str = std::str::from_utf8(body).unwrap_or("");
    let mut reader = Reader::from_str(body_str);
    reader.config_mut().trim_text(true);

    let mut is_sync_collection = false;
    let mut in_sync_token = false;
    let mut token_text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(e)) => {
                let local_name = e.local_name();
                let local = std::str::from_utf8(local_name.as_ref()).unwrap_or("");
                if local == "sync-collection" {
                    is_sync_collection = true;
                } else if local == "sync-token" && is_sync_collection {
                    in_sync_token = true;
                }
            }
            Ok(Event::Text(e)) if in_sync_token => {
                token_text = e.unescape().unwrap_or_default().into_owned();
                in_sync_token = false;
            }
            Ok(Event::End(e)) => {
                if std::str::from_utf8(e.local_name().as_ref()).unwrap_or("") == "sync-token" {
                    in_sync_token = false;
                }
            }
            _ => {}
        }
    }

    let since_mtime = token_text
        .strip_prefix(SYNC_TOKEN_PREFIX)
        .and_then(|s| s.parse::<i64>().ok());

    SyncRequest {
        since_mtime,
        is_sync_collection,
    }
}

// ─── Response builder ─────────────────────────────────────────────────────────

/// Build a `207 Multi-Status` response for a `sync-collection` REPORT.
///
/// - `url_prefix`: the stripped DAV mount prefix, e.g. `/dav/files/alice`.
///   Used to construct `<d:href>` values.
/// - `fc_base`: the `oc_filecache` path of the collection being synced,
///   e.g. `files` or `files/Photos`.
/// - `since_mtime`: the mtime threshold from the client's token, or `None`
///   for an initial sync (returns all nodes).
pub async fn build_sync_response(
    state: &NcDavState,
    uid: &str,
    storage_id: i64,
    url_prefix: &str,
    fc_base: &str,
    since_mtime: Option<i64>,
) -> Response<Body> {
    // ── Query changed nodes ───────────────────────────────────────────────
    // Pass -1 for initial sync so `mtime > -1` returns all rows.
    let rows = row::list_changed_since(
        &state.pool,
        &state.table_prefix,
        storage_id,
        fc_base,
        since_mtime.unwrap_or(-1),
    )
    .await;

    // ── Compute new sync token ────────────────────────────────────────────
    let new_max_mtime = row::get_subtree_max_mtime(
        &state.pool,
        &state.table_prefix,
        storage_id,
        fc_base,
    )
    .await;
    let new_token = format!("{SYNC_TOKEN_PREFIX}{new_max_mtime}");

    // ── Resolve shared context ────────────────────────────────────────────
    let data_fingerprint = {
        let cache = state.appconfig_cache.read().expect("appconfig lock");
        cache
            .get_string("core", "data-fingerprint")
            .unwrap_or_default()
    };
    let owner_display_name =
        row::lookup_user_display_name(&state.pool, &state.table_prefix, uid).await;
    let instance_id = state.instance_id.as_str();

    // Batch-load oc_filecache_extended for all changed nodes in one query
    // so that {nc:}creation_time, {nc:}upload_time, metadata_etag are correct.
    let fileids: Vec<i64> = rows.iter().map(|r| r.fileid).collect();
    let ext_map = row::list_extended_batch(&state.pool, &state.table_prefix, &fileids).await;

    // ── Build 207 Multi-Status XML ────────────────────────────────────────
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:multistatus xmlns:d=\"DAV:\" \
         xmlns:oc=\"http://owncloud.org/ns\" \
         xmlns:nc=\"http://nextcloud.org/ns\" \
         xmlns:ocs=\"http://open-collaboration-services.org/ns\">\n",
    );

    for fc_row in &rows {
        // Resolve MIME type from cache — no per-row DB query.
        let mime_str = {
            let cache = state.mime_cache.read().expect("mime cache lock");
            cache
                .get_name(fc_row.mimetype)
                .unwrap_or("application/octet-stream")
                .to_string()
        };
        let is_dir = mime_str == "httpd/unix-directory";

        // Apply authoritative extended times.
        let (creation_time, upload_time, metadata_etag) =
            if let Some(ext) = ext_map.get(&fc_row.fileid) {
                let ct = if ext.creation_time > 0 {
                    ext.creation_time
                } else {
                    fc_row.creation_time
                };
                let ut = if ext.upload_time > 0 {
                    ext.upload_time
                } else {
                    fc_row.upload_time
                };
                (ct, ut, ext.metadata_etag.clone())
            } else {
                (fc_row.creation_time, fc_row.upload_time, None)
            };

        // Build <d:href>
        let fc_path = fc_row.path.as_deref().unwrap_or("");
        let dav_rel = fc_path_to_dav_rel(fc_path);
        let href = if dav_rel.is_empty() {
            // Root collection itself
            format!("{url_prefix}/")
        } else {
            format!("{url_prefix}/{}", percent_encode_path(&dav_rel))
        };

        let etag = fc_row.etag.as_deref().unwrap_or("");
        let oc_id = format!("{:08}{instance_id}", fc_row.fileid);
        let can_rename = fc_row.permissions & 2 != 0;
        let perms_str = props::encode_permissions(fc_row.permissions, is_dir, false, false, can_rename);

        // Dates
        let last_modified = fmt_http_date(fc_row.mtime);
        let created = fmt_iso8601(creation_time);

        // ── <d:response> ──────────────────────────────────────────────────
        let _ = write!(xml, "  <d:response>\n    <d:href>{href}</d:href>\n");
        xml += "    <d:propstat>\n      <d:prop>\n";

        // resourcetype
        if is_dir {
            xml += "        <d:resourcetype><d:collection/></d:resourcetype>\n";
        } else {
            xml += "        <d:resourcetype/>\n";
            let _ = write!(
                xml,
                "        <d:getcontentlength>{}</d:getcontentlength>\n",
                fc_row.size.max(0)
            );
            let _ = write!(xml, "        <d:getcontenttype>{mime_str}</d:getcontenttype>\n");
        }

        // Standard DAV
        let _ = write!(xml, "        <d:getetag>\"{etag}\"</d:getetag>\n");
        let _ = write!(
            xml,
            "        <d:getlastmodified>{last_modified}</d:getlastmodified>\n"
        );
        let _ = write!(xml, "        <d:creationdate>{created}</d:creationdate>\n");

        // OC namespace
        let _ = write!(xml, "        <oc:id>{oc_id}</oc:id>\n");
        let _ = write!(xml, "        <oc:fileid>{}</oc:fileid>\n", fc_row.fileid);
        let _ = write!(xml, "        <oc:permissions>{perms_str}</oc:permissions>\n");
        let _ = write!(xml, "        <oc:size>{}</oc:size>\n", fc_row.size.max(0));
        let _ = write!(xml, "        <oc:etag>{etag}</oc:etag>\n");
        match &fc_row.checksum {
            Some(cs) if !cs.is_empty() => {
                let _ = write!(
                    xml,
                    "        <oc:checksums>\
                     <oc:checksum xmlns:oc=\"http://owncloud.org/ns\">{cs}</oc:checksum>\
                     </oc:checksums>\n"
                );
            }
            _ => {
                xml += "        <oc:checksums/>\n";
            }
        }
        let _ = write!(
            xml,
            "        <oc:owner-id>{uid}</oc:owner-id>\n"
        );
        let _ = write!(
            xml,
            "        <oc:owner-display-name>{}</oc:owner-display-name>\n",
            xml_escape(&owner_display_name)
        );
        let _ = write!(
            xml,
            "        <oc:data-fingerprint>{data_fingerprint}</oc:data-fingerprint>\n"
        );
        xml += "        <ocs:share-permissions>31</ocs:share-permissions>\n";

        // NC namespace
        xml += "        <nc:has-preview>false</nc:has-preview>\n";
        xml += "        <nc:mount-type>local</nc:mount-type>\n";
        xml += "        <nc:is-federated>false</nc:is-federated>\n";
        xml += "        <nc:hide-download>false</nc:hide-download>\n";
        let _ = write!(xml, "        <nc:creation_time>{creation_time}</nc:creation_time>\n");
        let _ = write!(xml, "        <nc:upload_time>{upload_time}</nc:upload_time>\n");
        if let Some(ref meta_etag) = metadata_etag {
            let _ = write!(xml, "        <nc:metadata_etag>{meta_etag}</nc:metadata_etag>\n");
        }

        xml += "      </d:prop>\n";
        xml += "      <d:status>HTTP/1.1 200 OK</d:status>\n";
        xml += "    </d:propstat>\n";
        xml += "  </d:response>\n";
    }

    // Trailing sync-token
    let _ = write!(xml, "  <d:sync-token>{new_token}</d:sync-token>\n</d:multistatus>\n");

    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header("Content-Type", "application/xml; charset=utf-8")
        .header("Content-Security-Policy", "default-src 'none';")
        .body(Body::from(xml))
        .unwrap()
}

// ─── Path helpers ─────────────────────────────────────────────────────────────

/// Convert a filecache path (`files/Photos/img.jpg`) to the DAV-relative
/// portion (`Photos/img.jpg`) that follows the stripped mount prefix.
///
/// The `files/` root maps to an empty string (the collection itself).
fn fc_path_to_dav_rel(fc_path: &str) -> String {
    match fc_path.strip_prefix("files/") {
        Some(rel) => rel.to_string(),
        None if fc_path == "files" || fc_path.is_empty() => String::new(),
        None => fc_path.to_string(),
    }
}

/// Percent-encode a relative path for embedding in `<d:href>`.
///
/// Encodes every byte that is not a safe URI-path character.  `/` is left
/// unencoded so that path separators are preserved.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
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
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

// ─── Date formatters ──────────────────────────────────────────────────────────

/// Format a Unix timestamp as an RFC 1123 HTTP date (for `{DAV:}getlastmodified`).
fn fmt_http_date(unix_ts: i64) -> String {
    let st = UNIX_EPOCH + Duration::from_secs(unix_ts.max(0) as u64);
    httpdate::fmt_http_date(st)
}

/// Format a Unix timestamp as an ISO 8601 / RFC 3339 UTC datetime
/// (for `{DAV:}creationdate`).
///
/// Uses the Hinnant algorithm (days-since-epoch → Y/M/D) which is correct for
/// the full Gregorian proleptic calendar.
fn fmt_iso8601(unix_ts: i64) -> String {
    let ts = unix_ts.max(0) as u64;
    let (year, month, day) = days_to_ymd(ts / 86400);
    let secs = ts % 86400;
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Convert days-since-Unix-epoch to (year, month, day).
///
/// Implements the Hinnant algorithm from
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>.
fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32)
}

// ─── XML helper ───────────────────────────────────────────────────────────────

/// Escape characters unsafe in XML text content.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── days_to_ymd ───────────────────────────────────────────────────────────

    #[test]
    fn unix_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn jan_15_2024() {
        // 2024-01-15: days since epoch = 19737
        assert_eq!(days_to_ymd(19737), (2024, 1, 15));
    }

    #[test]
    fn leap_day_2000() {
        // 2000-02-29: 11016 days since epoch
        assert_eq!(days_to_ymd(11016), (2000, 2, 29));
    }

    // ── fmt_iso8601 ───────────────────────────────────────────────────────────

    #[test]
    fn iso8601_epoch() {
        assert_eq!(fmt_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_known_ts() {
        // python3: datetime(2024,1,15,12,34,56,tzinfo=timezone.utc).timestamp() = 1705322096
        assert_eq!(fmt_iso8601(1705322096), "2024-01-15T12:34:56Z");
    }

    // ── parse_report_body ─────────────────────────────────────────────────────

    #[test]
    fn parse_sync_collection_with_token() {
        let body = br#"<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token>http://sabre.io/ns/sync/1705322096</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>"#;
        let req = parse_report_body(body);
        assert!(req.is_sync_collection);
        assert_eq!(req.since_mtime, Some(1705322096));
    }

    #[test]
    fn parse_sync_collection_empty_token() {
        let body = br#"<d:sync-collection xmlns:d="DAV:">
  <d:sync-token></d:sync-token>
</d:sync-collection>"#;
        let req = parse_report_body(body);
        assert!(req.is_sync_collection);
        assert_eq!(req.since_mtime, None);
    }

    #[test]
    fn parse_principal_property_search_not_sync() {
        let body = br#"<d:principal-property-search xmlns:d="DAV:">
  <d:property-search/>
</d:principal-property-search>"#;
        let req = parse_report_body(body);
        assert!(!req.is_sync_collection);
        assert_eq!(req.since_mtime, None);
    }

    // ── fc_path_to_dav_rel ────────────────────────────────────────────────────

    #[test]
    fn fc_root_maps_to_empty() {
        assert_eq!(fc_path_to_dav_rel("files"), "");
    }

    #[test]
    fn fc_file_maps_correctly() {
        assert_eq!(fc_path_to_dav_rel("files/Photos/img.jpg"), "Photos/img.jpg");
    }

    // ── xml_escape ────────────────────────────────────────────────────────────

    #[test]
    fn xml_escapes_special_chars() {
        assert_eq!(xml_escape("a&b<c>d\"e"), "a&amp;b&lt;c&gt;d&quot;e");
    }

    #[test]
    fn xml_escape_plain_passthrough() {
        assert_eq!(xml_escape("Alice Test"), "Alice Test");
    }
}
