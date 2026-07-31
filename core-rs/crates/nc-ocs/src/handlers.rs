use axum::{
    extract::{Request, State},
    http::HeaderMap,
    response::Response,
};
use nc_auth::AuthInfo;

use crate::{
    capabilities::SharedCapabilityCache,
    envelope::{into_axum_response, negotiate_format, OcsVersion},
    OcsState,
};

/// `GET /ocs/v1.php/config` and `GET /ocs/v2.php/config`
///
/// REQ §5.6: returns `version: "1.7"`, `website`, `host`, `contact`, `ssl`.
pub async fn ocs_config(State(state): State<OcsState>, request: Request) -> Response {
    let version = OcsVersion::from_path(request.uri().path());
    let format = negotiate_format(request.uri().query(), header_str(request.headers(), "accept"));

    let host = request
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let ssl = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|p| p == "https")
        .unwrap_or(false);

    let contact = state
        .appconfig_cache
        .read()
        .expect("appconfig lock")
        .get_string("core", "adminContact")
        .unwrap_or_default();

    let data = serde_json::json!({
        "version": "1.7",
        "website": "Nextcloud",
        "host": host,
        "contact": contact,
        "ssl": ssl,
    });

    into_axum_response(version, format, 100, "OK", data)
}

/// `GET /ocs/v1.php/cloud/capabilities` and `GET /ocs/v2.php/cloud/capabilities`
///
/// REQ §5.6: returns pre-built payload from `CapabilityCache`.
/// ETag = `md5(json_encode($result))`.
/// Unauthenticated → public-only subset; authenticated → full set.
pub async fn ocs_capabilities(
    State(state): State<OcsState>,
    request: Request,
) -> Response {
    let version = OcsVersion::from_path(request.uri().path());
    let format = negotiate_format(request.uri().query(), header_str(request.headers(), "accept"));

    let cache = state
        .capability_cache
        .read()
        .expect("capability cache lock poisoned");

    // Check whether the request is authenticated.  The auth middleware inserts
    // `AuthInfo` as an extension (§7.2) only when credentials are valid; its
    // absence means unauthenticated.  (The middleware inserts `AuthInfo`
    // directly — not `Option<AuthInfo>` — matching every other consumer, e.g.
    // nc-dav/src/handler.rs and nc-fastcgi/src/lib.rs.)
    let is_authenticated = request.extensions().get::<AuthInfo>().is_some();

    let (body_json, body_xml, etag) = if is_authenticated {
        (
            cache.auth_json.clone(),
            cache.auth_xml.clone(),
            cache.auth_etag.clone(),
        )
    } else {
        (
            cache.public_json.clone(),
            cache.public_xml.clone(),
            cache.public_etag.clone(),
        )
    };
    drop(cache);

    use axum::{body::Body, http::StatusCode, response::Response as AxumResponse};

    let http_status = match version {
        OcsVersion::V1 => StatusCode::OK,
        OcsVersion::V2 => StatusCode::OK,
    };

    // Wrap pre-built payload in the OCS envelope for the requested format.
    let data: serde_json::Value = serde_json::from_str(&body_json).unwrap_or(serde_json::json!({}));

    let (content_type, body_bytes) = match format {
        crate::envelope::OcsFormat::Json => {
            let envelope = crate::envelope::build_json(version, 100, "OK", data);
            (
                "application/json; charset=utf-8",
                serde_json::to_string(&envelope).unwrap(),
            )
        }
        crate::envelope::OcsFormat::Xml => {
            let xml = crate::envelope::build_xml(version, 100, "OK", &body_xml);
            ("text/xml; charset=UTF-8", xml)
        }
    };

    AxumResponse::builder()
        .status(http_status)
        .header("Content-Type", content_type)
        .header("ETag", format!("\"{etag}\""))
        .body(Body::from(body_bytes))
        .expect("valid capabilities response")
}

/// Helper: get a header value as `&str`.
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Rebuild the capability cache after a config change.
///
/// Re-computes the native portion from `appconfig_cache` and re-merges the
/// existing `php_app_capabilities` so that a config write (e.g. forbidden
/// filename list update) does not silently drop the PHP-app capability block.
///
/// To also refresh the PHP-app capabilities (e.g. after a PHP app is enabled),
/// call `apply_php_capabilities` and `apply_php_public_capabilities` on the
/// resulting cache directly after this.
pub async fn rebuild_capability_cache(
    appconfig_cache: &nc_db::appconfig::SharedAppConfigCache,
    capability_cache: &SharedCapabilityCache,
) {
    // Snapshot both PHP capability sources and version before rebuilding so we
    // can re-merge them.  The version is immutable for the server's lifetime
    // (only changes on upgrade, which requires restart).
    let (existing_php_caps, existing_php_pub_caps, existing_version) = {
        let r = capability_cache.read().expect("capability cache read lock");
        (
            r.php_app_capabilities.clone(),
            r.php_public_capabilities.clone(),
            r.version.clone(),
        )
    };

    let new_cache = {
        let ac = appconfig_cache.read().expect("appconfig lock");
        let mut cache =
            crate::capabilities::build_capability_cache(&ac, Some(&existing_version));
        // Re-merge both PHP capability sources into the freshly built native cache.
        if !existing_php_caps.is_null() {
            cache.apply_php_capabilities(existing_php_caps);
        }
        if !existing_php_pub_caps.is_null() {
            cache.apply_php_public_capabilities(existing_php_pub_caps);
        }
        cache
    };

    let mut cap = capability_cache.write().expect("capability cache write lock");
    *cap = new_cache;
    tracing::debug!("Capability cache rebuilt (PHP-app capabilities preserved)");
}
