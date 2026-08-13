//! `filter-files` REPORT handler (Phase 9.8).
//!
//! PHP reference: `apps/dav/lib/Connector/Sabre/FilesReportPlugin.php`.
//!
//! Handles the `{http://owncloud.org/ns}filter-files` REPORT on the files tree.
//! The web/iOS client uses this to populate Favorites, Tags, and Recent views
//! without a separate endpoint — it's a PROPFIND-like query with filter rules.
//!
//! Currently supported filter rules:
//! - `{oc:}favorite` — presence-based; returns files favorited by the user.

use axum::body::Body;
use dav_server::fs::DavProp;
use http::StatusCode;
use nc_db::pool::DbPool;
use xmltree::Element;

use crate::metadata::NcMetaData;
use crate::row;

// ─── Public entry point ───────────────────────────────────────────────────────

/// Handle a REPORT request body.  Returns `Some(response)` if this was a
/// `filter-files` REPORT and was handled; `None` if the REPORT type is
/// unrecognised (caller should return an appropriate error).
pub async fn handle_report(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    _storage_id: i64,
    _base_url: &str,
    instance_id: &str,
    mime_cache: &crate::SharedMimeCache,
    body_bytes: &[u8],
) -> Option<http::Response<Body>> {
    // Parse the XML body.
    let root = match Element::parse(body_bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "REPORT: failed to parse XML body");
            return Some(build_xml_error(400, "Bad Request", "Invalid XML body"));
        }
    };

    // Only handle {oc:}filter-files.
    if root.name != "filter-files"
        || root.namespace.as_deref().unwrap_or("") != "http://owncloud.org/ns"
    {
        return None; // Not a filter-files REPORT.
    }

    tracing::debug!(uid, "handling filter-files REPORT");

    // ── Extract filter rules and requested properties ────────────────────────
    let ns_oc = "http://owncloud.org/ns";
    let ns_nc = "http://nextcloud.org/ns";
    let ns_dav = "DAV:";

    let mut filter_favorite = false;
    let mut requested_props: Vec<(String, String)> = Vec::new(); // (namespace, name)
    let mut limit: Option<usize> = None;
    let mut offset: Option<usize> = None;

    for child in &root.children {
        let el = match child.as_element() {
            Some(e) => e,
            None => continue,
        };
        let child_ns = el.namespace.as_deref().unwrap_or("");

        match (child_ns, el.name.as_str()) {
            ("http://owncloud.org/ns", "filter-rules") => {
                for rule_child in &el.children {
                    let rule = match rule_child.as_element() {
                        Some(r) => r,
                        None => continue,
                    };
                    let rule_ns = rule.namespace.as_deref().unwrap_or("");
                    if rule_ns == ns_oc && rule.name == "favorite" {
                        filter_favorite = true;
                    }
                }
            }
            ("DAV:", "prop") => {
                for prop_child in &el.children {
                    let prop = match prop_child.as_element() {
                        Some(p) => p,
                        None => continue,
                    };
                    requested_props.push((
                        prop.namespace.clone().unwrap_or_default(),
                        prop.name.clone(),
                    ));
                }
            }
            ("DAV:", "limit") => {
                for limit_child in &el.children {
                    let lc = match limit_child.as_element() {
                        Some(l) => l,
                        None => continue,
                    };
                    let lc_ns = lc.namespace.as_deref().unwrap_or("");
                    if lc_ns == ns_dav && lc.name == "nresults" {
                        limit = lc.get_text().and_then(|t| t.parse().ok());
                    } else if lc_ns == ns_nc && lc.name == "firstresult" {
                        offset = lc.get_text().and_then(|t| t.parse().ok());
                    }
                }
            }
            _ => {}
        }
    }

    // Empty filter rules → 400.
    if !filter_favorite {
        return Some(build_xml_error(
            400,
            "Bad Request",
            "Missing filter-rule block in request",
        ));
    }

    // ── Query matching files ─────────────────────────────────────────────────
    let favorite_ids = row::get_favorite_fileids(pool, prefix, uid).await;
    if favorite_ids.is_empty() {
        return Some(build_empty_multistatus());
    }

    // Apply offset/limit.
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(usize::MAX);
    let paged_ids: Vec<i64> = favorite_ids
        .iter()
        .skip(offset)
        .take(limit)
        .copied()
        .collect();

    if paged_ids.is_empty() {
        return Some(build_empty_multistatus());
    }

    // Batch-lookup filecache rows.
    let fc_map = row::lookup_by_ids(pool, prefix, &paged_ids).await;

    // Batch-lookup extended data.
    let extended_map = row::list_extended_batch(pool, prefix, &paged_ids).await;

    // Read MIME cache.
    let mime_guard = mime_cache.read().expect("mime cache lock");

    // Build a response for each matching file.
    let mut responses_xml = String::new();
    let dav_base = format!("/remote.php/dav/files/{uid}");

    for &fid in &paged_ids {
        let fc_row = match fc_map.get(&fid) {
            Some(r) => r,
            None => continue,
        };

        let mime_type = mime_guard
            .get_name(fc_row.mimetype)
            .unwrap_or_else(|| std::sync::Arc::from("application/octet-stream"));

        let mut meta = NcMetaData::from_row(fc_row, mime_type, None);
        if let Some(ext) = extended_map.get(&fid) {
            meta.apply_extended(
                ext.creation_time,
                ext.upload_time,
                ext.metadata_etag.clone(),
            );
        }

        // Build the DAV href.
        let dav_path = fc_row
            .path
            .as_deref()
            .map(|p| p.strip_prefix("files").unwrap_or(p))
            .unwrap_or("");
        let href = if dav_path.is_empty() || dav_path == "/" {
            format!("{dav_base}/")
        } else {
            format!("{dav_base}{dav_path}")
        };

        // Build props for this file.
        let props = crate::props::build_props(
            &meta,
            instance_id,
            uid,
            uid,                // owner_display_name — will be resolved per-file
            true,               // do_content
            "",                 // data_fingerprint
            0,                  // child_dir_count
            0,                  // child_file_count
            false,              // is_mounted
            false,              // is_shared
            fc_row.permissions, // share_permissions (use raw — sharing mask not applied for now)
            "",                 // download_url
            "",                 // note
            false,              // has_preview
            &[],                // tags
            true,               // favorite
        );

        // Group props: Some(xml) → 200, None(xml) → 404.
        let mut props_200: Vec<&[u8]> = Vec::new();
        let mut props_404_names: Vec<String> = Vec::new();

        for prop in &props {
            match &prop.xml {
                Some(xml) if !xml.is_empty() => {
                    // Only include if requested (or if no explicit prop filter).
                    if requested_props.is_empty() || prop_is_requested(prop, &requested_props) {
                        props_200.push(xml.as_slice());
                    }
                }
                _ => {
                    if requested_props.is_empty() || prop_is_requested(prop, &requested_props) {
                        // 404 props: emit as self-closing elements.
                        props_404_names.push(prop_name_tag(prop));
                    }
                }
            }
        }

        // Also add any requested props that build_props didn't emit at all.
        if !requested_props.is_empty() {
            for (ns, name) in &requested_props {
                // Check if already covered by a 200 or 404 prop.
                let covered = props.iter().any(|p| {
                    p.namespace.as_deref().unwrap_or("") == ns.as_str() && p.name == *name
                });
                if !covered {
                    let tag = make_empty_tag(ns, name);
                    if !props_404_names.contains(&tag) {
                        props_404_names.push(tag);
                    }
                }
            }
        }

        // Write <d:response>.
        responses_xml.push_str("<d:response>");
        responses_xml.push_str(&format!("<d:href>{href}</d:href>"));

        // 200 propstat.
        if !props_200.is_empty() {
            responses_xml.push_str("<d:propstat><d:prop>");
            for xml_bytes in &props_200 {
                if let Ok(s) = std::str::from_utf8(xml_bytes) {
                    responses_xml.push_str(s);
                }
            }
            responses_xml.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>");
        }

        // 404 propstat.
        if !props_404_names.is_empty() {
            responses_xml.push_str("<d:propstat><d:prop>");
            for tag in &props_404_names {
                responses_xml.push_str(tag);
            }
            responses_xml
                .push_str("</d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>");
        }

        responses_xml.push_str("</d:response>");
    }

    let multistatus = format!(
        "<?xml version=\"1.0\"?>\n\
         <d:multistatus \
         xmlns:d=\"DAV:\" \
         xmlns:s=\"http://sabredav.org/ns\" \
         xmlns:oc=\"http://owncloud.org/ns\" \
         xmlns:nc=\"http://nextcloud.org/ns\" \
         xmlns:ocs=\"http://open-collaboration-services.org/ns\" \
         xmlns:ocm=\"http://open-cloud-mesh.org/ns\">\
         {responses_xml}\
         </d:multistatus>"
    );

    Some(
        http::Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/xml; charset=utf-8"),
            )
            .body(Body::from(multistatus))
            .unwrap(),
    )
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn build_xml_error(status: u16, exception: &str, message: &str) -> http::Response<Body> {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n  \
         <s:exception>{exception}</s:exception>\n  \
         <s:message>{message}</s:message>\n\
         </d:error>\n"
    );
    http::Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST))
        .header(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/xml; charset=utf-8"),
        )
        .body(Body::from(body))
        .unwrap()
}

fn build_empty_multistatus() -> http::Response<Body> {
    let body = "<?xml version=\"1.0\"?>\n<d:multistatus \
                xmlns:d=\"DAV:\" \
                xmlns:s=\"http://sabredav.org/ns\" \
                xmlns:oc=\"http://owncloud.org/ns\" \
                xmlns:nc=\"http://nextcloud.org/ns\" \
                xmlns:ocs=\"http://open-collaboration-services.org/ns\" \
                xmlns:ocm=\"http://open-cloud-mesh.org/ns\"/>";
    http::Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/xml; charset=utf-8"),
        )
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn prop_is_requested(prop: &DavProp, requested: &[(String, String)]) -> bool {
    let ns = prop.namespace.as_deref().unwrap_or("");
    requested
        .iter()
        .any(|(req_ns, req_name)| req_ns == ns && req_name == &prop.name)
}

/// Build a self-closing empty tag for a 404 prop, matching namespace conventions.
fn make_empty_tag(ns: &str, name: &str) -> String {
    match ns {
        "DAV:" => format!("<d:{name}/>"),
        "http://owncloud.org/ns" => format!("<oc:{name}/>"),
        "http://nextcloud.org/ns" => format!("<nc:{name}/>"),
        "http://open-collaboration-services.org/ns" => format!("<ocs:{name}/>"),
        "http://open-cloud-mesh.org/ns" => format!("<ocm:{name}/>"),
        _ => format!("<x:{name} xmlns:x=\"{ns}\"/>"),
    }
}

fn prop_name_tag(prop: &DavProp) -> String {
    let ns = prop.namespace.as_deref().unwrap_or("");
    let name = &prop.name;
    make_empty_tag(ns, name)
}
