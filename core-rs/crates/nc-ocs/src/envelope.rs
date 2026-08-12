/// OCS API version — drives HTTP status mapping and meta-block shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcsVersion {
    V1,
    V2,
}

impl OcsVersion {
    /// Detect from a request URI path (`/ocs/v1.php/…` vs `/ocs/v2.php/…`).
    pub fn from_path(path: &str) -> Self {
        if path.contains("/v2.php/") {
            OcsVersion::V2
        } else {
            OcsVersion::V1
        }
    }
}

/// Wire format requested by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcsFormat {
    Xml,
    Json,
}

/// Negotiate the response format.
///
/// Priority (REQ §5):
/// 1. `?format=json` or `?format=xml` query parameter
/// 2. `Accept: application/json` header
/// 3. Default: XML
pub fn negotiate_format(query: Option<&str>, accept: Option<&str>) -> OcsFormat {
    // 1. Query parameter takes precedence.
    if let Some(q) = query {
        for part in q.split('&') {
            if let Some(v) = part.strip_prefix("format=") {
                return match v {
                    "json" => OcsFormat::Json,
                    _ => OcsFormat::Xml,
                };
            }
        }
    }
    // 2. Accept header.
    if let Some(a) = accept {
        if a.contains("application/json") {
            return OcsFormat::Json;
        }
    }
    OcsFormat::Xml
}

/// OCS status code 100 = success; anything else is an error.
fn status_label(statuscode: u16) -> &'static str {
    if statuscode == 100 {
        "ok"
    } else {
        "failure"
    }
}

/// Map an OCS status code to an HTTP status code for v1.
///
/// v1 always returns HTTP 200 except for OCS 997 (unauthorised) → 401.
pub fn v1_http_status(statuscode: u16) -> u16 {
    if statuscode == 997 {
        401
    } else {
        200
    }
}

/// Map an OCS status code to an HTTP status code for v2.
pub fn v2_http_status(statuscode: u16) -> u16 {
    match statuscode {
        200..=299 => statuscode,
        997 => 401,
        998 => 404,
        999 => 500,
        s if s < 200 || s > 600 => 400,
        s => s,
    }
}

/// Build an OCS XML envelope string.
///
/// * v1: includes `<totalitems></totalitems>` and `<itemsperpage></itemsperpage>`.
/// * v2: omits those elements.
pub fn build_xml(version: OcsVersion, statuscode: u16, message: &str, data_xml: &str) -> String {
    let status = status_label(statuscode);
    let msg_escaped = xml_escape(message);
    let meta_extra = match version {
        OcsVersion::V1 => "\n  <totalitems></totalitems>\n  <itemsperpage></itemsperpage>",
        OcsVersion::V2 => "",
    };
    format!(
        "<?xml version=\"1.0\"?>\n\
         <ocs>\n\
          <meta>\n\
           <status>{status}</status>\n\
           <statuscode>{statuscode}</statuscode>\n\
           <message>{msg_escaped}</message>{meta_extra}\n\
          </meta>\n\
          <data>{data_xml}</data>\n\
         </ocs>"
    )
}

/// Build an OCS JSON envelope.
///
/// v1: `totalitems` and `itemsperpage` present as empty strings.
/// v2: those keys are absent.
pub fn build_json(
    version: OcsVersion,
    statuscode: u16,
    message: &str,
    data: serde_json::Value,
) -> serde_json::Value {
    let status = status_label(statuscode);
    let mut meta = serde_json::json!({
        "status": status,
        "statuscode": statuscode,
        "message": message,
    });
    if version == OcsVersion::V1 {
        meta["totalitems"] = serde_json::Value::String(String::new());
        meta["itemsperpage"] = serde_json::Value::String(String::new());
    }
    serde_json::json!({ "ocs": { "meta": meta, "data": data } })
}

/// Build a complete `axum::response::Response` from an OCS response.
pub fn into_axum_response(
    version: OcsVersion,
    format: OcsFormat,
    statuscode: u16,
    message: &str,
    data_json: serde_json::Value,
) -> axum::response::Response {
    use axum::{body::Body, http::StatusCode, response::Response};

    let http_status = match version {
        OcsVersion::V1 => v1_http_status(statuscode),
        OcsVersion::V2 => v2_http_status(statuscode),
    };
    let status = StatusCode::from_u16(http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    match format {
        OcsFormat::Json => {
            let body = build_json(version, statuscode, message, data_json);
            Response::builder()
                .status(status)
                .header("Content-Type", "application/json; charset=utf-8")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .expect("valid response")
        }
        OcsFormat::Xml => {
            let data_xml = json_to_xml_data(&data_json);
            let xml = build_xml(version, statuscode, message, &data_xml);
            Response::builder()
                .status(status)
                .header("Content-Type", "text/xml; charset=UTF-8")
                .body(Body::from(xml))
                .expect("valid response")
        }
    }
}

/// Minimal XML escaping for text content.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Convert a serde_json Value to inner XML content for the `<data>` element.
///
/// Follows Nextcloud PHP's array-to-XML serialisation convention:
/// - Objects → named child elements
/// - Arrays  → repeated elements using the parent key name
/// - Scalars → text nodes
/// - null/false/empty array → self-closing element at parent level
pub fn json_to_xml_data(value: &serde_json::Value) -> String {
    json_value_to_xml(value, "")
}

fn json_value_to_xml(value: &serde_json::Value, _parent_key: &str) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = String::new();
            for (k, v) in map {
                // Sanitise key: replace hyphens with underscores for XML element names
                // (Nextcloud uses hyphens in JSON keys like "webdav-root").
                let elem = k.replace('-', "_");
                match v {
                    serde_json::Value::Array(arr) if arr.is_empty() => {
                        out.push_str(&format!("<{elem}/>"));
                    }
                    serde_json::Value::Array(arr) => {
                        for item in arr {
                            out.push_str(&format!(
                                "<{elem}>{}</{elem}>",
                                xml_escape(&scalar_str(item))
                            ));
                        }
                    }
                    serde_json::Value::Object(_) => {
                        out.push_str(&format!("<{elem}>{}</{elem}>", json_value_to_xml(v, k)));
                    }
                    serde_json::Value::Null => {
                        out.push_str(&format!("<{elem}/>"));
                    }
                    serde_json::Value::Bool(b) => {
                        let s = if *b { "1" } else { "0" };
                        out.push_str(&format!("<{elem}>{s}</{elem}>"));
                    }
                    _ => {
                        out.push_str(&format!("<{elem}>{}</{elem}>", xml_escape(&scalar_str(v))));
                    }
                }
            }
            out
        }
        _ => xml_escape(&scalar_str(value)),
    }
}

fn scalar_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => String::new(),
        _ => String::new(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_negotiation_query_wins() {
        assert_eq!(
            negotiate_format(Some("format=json"), Some("text/html")),
            OcsFormat::Json
        );
        assert_eq!(
            negotiate_format(Some("foo=bar&format=xml"), Some("application/json")),
            OcsFormat::Xml
        );
    }

    #[test]
    fn format_negotiation_accept_header() {
        assert_eq!(
            negotiate_format(None, Some("application/json")),
            OcsFormat::Json
        );
        assert_eq!(negotiate_format(None, None), OcsFormat::Xml);
    }

    #[test]
    fn v1_xml_has_totalitems_and_itemsperpage() {
        let xml = build_xml(OcsVersion::V1, 100, "OK", "<foo/>");
        assert!(
            xml.contains("<totalitems></totalitems>"),
            "v1 must have totalitems"
        );
        assert!(
            xml.contains("<itemsperpage></itemsperpage>"),
            "v1 must have itemsperpage"
        );
        assert!(xml.contains("<statuscode>100</statuscode>"));
        assert!(xml.contains("<status>ok</status>"));
    }

    #[test]
    fn v2_xml_omits_pagination_fields() {
        let xml = build_xml(OcsVersion::V2, 100, "OK", "<foo/>");
        assert!(!xml.contains("totalitems"), "v2 must NOT have totalitems");
        assert!(
            !xml.contains("itemsperpage"),
            "v2 must NOT have itemsperpage"
        );
    }

    #[test]
    fn v1_json_has_pagination_as_empty_strings() {
        let body = build_json(OcsVersion::V1, 100, "OK", serde_json::json!({}));
        let meta = &body["ocs"]["meta"];
        assert_eq!(meta["totalitems"], "");
        assert_eq!(meta["itemsperpage"], "");
    }

    #[test]
    fn v2_json_omits_pagination() {
        let body = build_json(OcsVersion::V2, 100, "OK", serde_json::json!({}));
        let meta = &body["ocs"]["meta"];
        assert!(meta.get("totalitems").is_none());
        assert!(meta.get("itemsperpage").is_none());
    }

    #[test]
    fn v1_http_status_always_200_except_997() {
        assert_eq!(v1_http_status(100), 200);
        assert_eq!(v1_http_status(404), 200);
        assert_eq!(v1_http_status(997), 401);
    }

    #[test]
    fn v2_http_status_maps_correctly() {
        assert_eq!(v2_http_status(200), 200);
        assert_eq!(v2_http_status(997), 401);
        assert_eq!(v2_http_status(998), 404);
        assert_eq!(v2_http_status(999), 500);
        assert_eq!(v2_http_status(100), 400); // outside 200-600
    }

    #[test]
    fn version_detected_from_path() {
        assert_eq!(OcsVersion::from_path("/ocs/v1.php/config"), OcsVersion::V1);
        assert_eq!(
            OcsVersion::from_path("/ocs/v2.php/cloud/capabilities"),
            OcsVersion::V2
        );
    }
}
