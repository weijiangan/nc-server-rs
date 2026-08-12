//! Minimal Nextcloud HTTP/WebDAV client used to replay identical operation
//! sequences against the SUT and the Oracle.
//!
//! Methods return the **raw** [`reqwest::Response`] so the scenario runner
//! (Phase 16.4) can normalize and diff status/headers/body. The core WebDAV
//! verbs plus the OCS share/user ops; the composite upload flows
//! (`chunked_upload_v2`, `bulk`) are composed from these verbs in the
//! scenario runner itself (Phase 16.10).

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, HOST};
use reqwest::{Client, Method, Response};

use crate::config::Instance;

/// User-Agent of a Nextcloud desktop client. PHP skips the browser CSRF flow
/// for non-browser agents, which keeps the OCS/web endpoints reachable with
/// plain basic auth.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) mirall/3.13.0 \
     (Nextcloud, linux ClientArchitecture: x86_64 OsArchitecture: x86_64)";

pub struct NextcloudClient {
    http: Client,
    base_url: String,
    host: String,
    user: String,
    pass: String,
}

/// Build a WebDAV/custom [`Method`] from its name (PROPFIND, MKCOL, …).
fn method(name: &str) -> Result<Method> {
    Method::from_bytes(name.as_bytes()).with_context(|| format!("invalid HTTP method {name:?}"))
}

impl NextcloudClient {
    pub fn new(inst: &Instance, user: &str, pass: &str) -> Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base_url: inst.base_url.trim_end_matches('/').to_string(),
            host: inst.host.clone(),
            user: user.to_string(),
            pass: pass.to_string(),
        })
    }

    /// Issue one request and return the raw response. `path` is appended to the
    /// base URL; the configured `Host` header and basic auth are always sent.
    pub async fn request(
        &self,
        m: Method,
        path: &str,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
    ) -> Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self
            .http
            .request(m.clone(), &url)
            .basic_auth(&self.user, Some(&self.pass))
            .header(HOST, &self.host);
        for (k, v) in headers.iter() {
            req = req.header(k.clone(), v.clone());
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        req.send().await.with_context(|| format!("{} {}", m, url))
    }

    // ── Core WebDAV verbs ────────────────────────────────────────────────────

    pub async fn propfind(&self, path: &str, depth: u32, body: Option<&str>) -> Result<Response> {
        let mut h = HeaderMap::new();
        h.insert("Depth", HeaderValue::from(depth));
        let body = body.map(|s| s.as_bytes().to_vec());
        self.request(method("PROPFIND")?, path, h, body).await
    }

    pub async fn proppatch(&self, path: &str, body: &str) -> Result<Response> {
        self.request(
            method("PROPPATCH")?,
            path,
            HeaderMap::new(),
            Some(body.as_bytes().to_vec()),
        )
        .await
    }

    pub async fn put(&self, path: &str, body: Vec<u8>, headers: HeaderMap) -> Result<Response> {
        self.request(Method::PUT, path, headers, Some(body)).await
    }

    pub async fn get(&self, path: &str) -> Result<Response> {
        self.request(Method::GET, path, HeaderMap::new(), None)
            .await
    }

    pub async fn delete(&self, path: &str, headers: HeaderMap) -> Result<Response> {
        self.request(Method::DELETE, path, headers, None).await
    }

    pub async fn mkcol(&self, path: &str) -> Result<Response> {
        self.request(method("MKCOL")?, path, HeaderMap::new(), None)
            .await
    }

    pub async fn move_(&self, from: &str, to: &str) -> Result<Response> {
        let mut h = HeaderMap::new();
        h.insert("Destination", destination_value(&self.base_url, to)?);
        self.request(method("MOVE")?, from, h, None).await
    }

    pub async fn copy(&self, from: &str, to: &str) -> Result<Response> {
        let mut h = HeaderMap::new();
        h.insert("Destination", destination_value(&self.base_url, to)?);
        self.request(method("COPY")?, from, h, None).await
    }

    // ── OCS shares (Phase 16.7) ─────────────────────────────────────────────

    /// OCS share create: `POST /ocs/v2.php/apps/files_sharing/api/v1/shares`.
    ///
    /// Share creation is proxied to PHP on the SUT (`nc-ocs` only serves
    /// `/config` and `/cloud/capabilities` natively), so this drives identical
    /// PHP code on both sides — the harness self-check. `format=json` plus the
    /// JSON `Accept` make the response machine-readable for value capture.
    pub async fn share_create(&self, params: &[(String, String)]) -> Result<Response> {
        let mut h = HeaderMap::new();
        h.insert("OCS-APIRequest", HeaderValue::from_static("true"));
        h.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        // Without an explicit form Content-Type PHP never populates $_POST and
        // the controller rejects with "Please specify a file or folder path".
        h.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let body = form_urlencoded(params);
        self.request(
            Method::POST,
            "/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json",
            h,
            Some(body.into_bytes()),
        )
        .await
    }

    /// OCS share delete: `DELETE /ocs/v2.php/apps/files_sharing/api/v1/shares/{id}`.
    /// Deleting a group share also removes its TYPE_USERGROUP child rows.
    pub async fn share_delete(&self, id: &str) -> Result<Response> {
        let mut h = HeaderMap::new();
        h.insert("OCS-APIRequest", HeaderValue::from_static("true"));
        self.request(
            Method::DELETE,
            &format!("/ocs/v2.php/apps/files_sharing/api/v1/shares/{id}?format=json"),
            h,
            None,
        )
        .await
    }

    /// OCS user update (Phase 16.10 quota scenarios):
    /// `PUT /ocs/v2.php/cloud/users/{user}` with `key`/`value` form params.
    /// Proxied to PHP on the SUT (`nc-ocs` serves only config + capabilities),
    /// so both sides run the same provisioning code.
    pub async fn ocs_user_update(&self, user: &str, key: &str, value: &str) -> Result<Response> {
        let mut h = HeaderMap::new();
        h.insert("OCS-APIRequest", HeaderValue::from_static("true"));
        h.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        h.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let body = form_urlencoded(&[
            ("key".to_string(), key.to_string()),
            ("value".to_string(), value.to_string()),
        ]);
        self.request(
            Method::PUT,
            &format!("/ocs/v2.php/cloud/users/{user}?format=json"),
            h,
            Some(body.into_bytes()),
        )
        .await
    }
}

/// Minimal `application/x-www-form-urlencoded` encoding (RFC 3986 unreserved
/// set passes through; everything else is percent-escaped).
fn form_urlencoded(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build a `Destination` header value (absolute URL) for MOVE/COPY.
fn destination_value(base_url: &str, to: &str) -> Result<HeaderValue> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), to);
    HeaderValue::from_str(&url).with_context(|| format!("invalid Destination {url:?}"))
}
