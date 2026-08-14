//! Client identity resolution — Phase 15 F2 (trusted proxies + client
//! identity).
//!
//! Line-for-line port of PHP's `lib/private/AppFramework/Http/Request.php`
//! (`getRemoteAddress`, `getServerProtocol`, `getInsecureServerHost`,
//! `getServerHost`, `fromTrustedProxy`, `isOverwriteCondition`,
//! `getOverwriteHost`), Symfony's `IpUtils::checkIp` CIDR matching, and the
//! `TrustedDomainHelper::isTrustedDomain` wildcard semantics, plus the
//! `lib/base.php:872-912` trusted-domain enforcement.
//!
//! Resolve ONCE per request (just inside the trace layer) into a
//! [`ClientIdentity`] request extension; auth, the FastCGI proxy, and the
//! SameSite cookie check all consume it.  The same resolution is handed to
//! PHP as `REMOTE_ADDR`; PHP runs the identical algorithm under the same
//! config and converges (the chained-proxy coherence argument).

use std::net::IpAddr;
// The test-only `const PEER` uses Ipv4Addr — import it for the test build
// only, so the production build doesn't warn "unused import" (the warning
// fires there because `#[cfg(test)] mod tests` isn't compiled).
#[cfg(test)]
use std::net::Ipv4Addr;

use axum::http::{HeaderMap, Response, StatusCode};
use nc_db::config::NcConfig;

/// The resolved client identity lives in `nc-auth` (shared with the FastCGI
/// proxy crate).
pub use nc_auth::ClientIdentity;

/// Resolve the client identity once per request (Phase 15 F2).  `peer_addr`
/// is the TCP peer address (`REMOTE_ADDR` in PHP terms).
pub fn resolve(headers: &HeaderMap, peer_addr: IpAddr, cfg: &NcConfig) -> ClientIdentity {
    let ip = remote_address(peer_addr, headers, cfg);
    let https = server_protocol(peer_addr, headers, cfg);
    let host = insecure_server_host(peer_addr, headers, cfg);
    let port = host_port(&host, https);
    ClientIdentity {
        ip,
        https,
        host,
        port,
    }
}

/// Header name PHP reads for forwarded-for entries.  Config values are
/// server-param names (`HTTP_X_FORWARDED_FOR`); strip the `HTTP_` prefix,
/// uppercase → lowercase, and map `_` → `-` to reach the wire header.
fn forwarded_header_wire_name(server_param: &str) -> Option<String> {
    let upper = server_param.to_ascii_uppercase();
    let rest = upper.strip_prefix("HTTP_")?;
    Some(rest.replace('_', "-").to_ascii_lowercase())
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `IpUtils::checkIp` — any entry matches (CIDR or exact), family-aware.
/// Malformed entries log + never match (PHP: `error_log` + false).
pub fn is_trusted_proxy(cfg: &NcConfig, ip: &IpAddr) -> bool {
    match cfg.trusted_proxies.as_deref() {
        None | Some([]) => false,
        Some(list) => list.iter().any(|entry| {
            cidr_matches(entry, *ip).unwrap_or_else(|| {
                tracing::warn!(entry = %entry, "trusted_proxies has a malformed entry");
                false
            })
        }),
    }
}

/// Exact or CIDR-prefix match of `entry` (v4/v6, same family as `ip`).
/// Mirrors `IpUtils::checkIp4`/`checkIp6` packed-binary prefix masking.
pub fn cidr_matches(entry: &str, ip: IpAddr) -> Option<bool> {
    let (addr, bits) = match entry.split_once('/') {
        Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
        None => (entry, None),
    };
    let parsed: IpAddr = addr.parse().ok()?;
    match (parsed, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => {
            let bits = bits.unwrap_or(32);
            if bits > 32 {
                return None;
            }
            let mask = if bits == 0 {
                0u32
            } else {
                u32::MAX << (32 - bits)
            };
            let a = u32::from(net) & mask;
            let b = u32::from(ip) & mask;
            Some(a == b)
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) => {
            let bits = bits.unwrap_or(128);
            if bits > 128 {
                return None;
            }
            let mask = if bits == 0 {
                0u128
            } else {
                u128::MAX << (128 - bits)
            };
            let a = u128::from(net) & mask;
            let b = u128::from(ip) & mask;
            Some(a == b)
        }
        _ => Some(false),
    }
}

/// `Request::getRemoteAddress` — the XFF walk.  Only honoured when the peer
/// is a trusted proxy; entries walked right-to-left, skipping entries that
/// are themselves trusted proxies; `[v6]:port` / `v4:port` forms stripped.
fn remote_address(peer: IpAddr, headers: &HeaderMap, cfg: &NcConfig) -> IpAddr {
    if !is_trusted_proxy(cfg, &peer) {
        return peer;
    }
    // PHP default: `['HTTP_X_FORWARDED_FOR']` — "only have one default, so we
    // cannot ship an insecure product out of the box".
    let forwarded_for_headers: Vec<String> = cfg
        .forwarded_for_headers
        .clone()
        .unwrap_or_else(|| vec!["HTTP_X_FORWARDED_FOR".to_string()]);

    // Read the headers and values in reverse order as per
    // https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/X-Forwarded-For#selecting_an_ip_address
    for server_param in forwarded_for_headers.iter().rev() {
        let Some(wire) = forwarded_header_wire_name(server_param) else {
            continue;
        };
        let Some(value) = header_value(headers, &wire) else {
            continue;
        };
        for raw in value.split(',').rev() {
            let mut candidate = raw.trim();
            let colons = candidate.matches(':').count();
            if colons > 1 {
                // Extract IP from string with brackets and optional port
                // (PHP: `/^\[(.+?)\](?::\d+)?$/`).
                if let Some(rest) = candidate.strip_prefix('[') {
                    if let Some(close) = rest.find(']') {
                        candidate = &rest[..close];
                    }
                }
            } else if colons == 1 {
                // IPv4 with port (PHP: `substr($IP, 0, strpos($IP, ':'))`).
                if let Some(idx) = candidate.find(':') {
                    candidate = &candidate[..idx];
                }
            }
            let Ok(ip) = candidate.parse::<IpAddr>() else {
                continue; // filter_var(IP, FILTER_VALIDATE_IP) === false
            };
            if is_trusted_proxy(cfg, &ip) {
                continue;
            }
            return ip;
        }
    }
    peer
}

/// `Request::isOverwriteCondition` — `preg_match('/'.overwritecondaddr.'/',
/// REMOTE_ADDR) === 1`; an empty value is `'//'`, which matches everything.
/// The pattern is PCRE in PHP; regex-lite covers the common admin patterns —
/// deviation documented: a pattern outside regex-lite's syntax logs and
/// behaves as no-match (PHP: `preg_match` error → `=== 1` false).
fn is_overwrite_condition(peer: IpAddr, cfg: &NcConfig) -> bool {
    let pattern = cfg.overwritecondaddr.as_deref().unwrap_or("");
    if pattern.is_empty() {
        return true;
    }
    let re = match regex_lite::Regex::new(pattern) {
        Ok(re) => re,
        Err(e) => {
            tracing::warn!(pattern = %pattern, error = %e, "overwritecondaddr regex invalid — condition treated as false");
            return false;
        }
    };
    re.is_match(&peer.to_string())
}

/// `Request::getServerProtocol` — https when `overwriteprotocol` (with the
/// overwrite condition) says so, else `X-Forwarded-Proto` (first of a comma
/// list) from a trusted proxy only, else the `HTTPS` server param.
///
/// (The `HTTPS` server param only exists when the webserver sets it; the
/// proxy derives it from the identity, so there is nothing more to read
/// here — `is_https` for the SameSite check comes from this function.)
fn server_protocol(peer: IpAddr, headers: &HeaderMap, cfg: &NcConfig) -> bool {
    let mut proto = "http".to_string();
    if let Some(overwrite) = cfg.overwriteprotocol.as_deref() {
        if !overwrite.is_empty() && is_overwrite_condition(peer, cfg) {
            proto = overwrite.to_lowercase();
        }
    } else if is_trusted_proxy(cfg, &peer) {
        if let Some(xfp) = header_value(headers, "x-forwarded-proto") {
            proto = xfp.split(',').next().unwrap_or("").trim().to_lowercase();
        }
    }
    if proto != "https" && proto != "http" {
        tracing::warn!(proto = %proto, "Server protocol is malformed — falling back to http (check overwriteprotocol / X-Forwarded-Proto)");
    }
    proto == "https"
}

/// `Request::getOverwriteHost` — `overwritehost` when set and the condition
/// matches.
fn get_overwrite_host(peer: IpAddr, cfg: &NcConfig) -> Option<String> {
    match cfg.overwritehost.as_deref() {
        Some(h) if !h.is_empty() && is_overwrite_condition(peer, cfg) => Some(h.to_string()),
        _ => None,
    }
}

/// `Request::getInsecureServerHost` — unverified host from headers.
fn insecure_server_host(peer: IpAddr, headers: &HeaderMap, cfg: &NcConfig) -> String {
    if is_trusted_proxy(cfg, &peer) {
        if let Some(host) = get_overwrite_host(peer, cfg) {
            return host;
        }
    }
    let mut host = "localhost".to_string();
    if is_trusted_proxy(cfg, &peer) {
        if let Some(xfh) = header_value(headers, "x-forwarded-host") {
            host = xfh.split(',').next().unwrap_or("").trim().to_string();
        }
    } else if let Some(h) = header_value(headers, "host") {
        host = h.to_string();
    }
    host
}

/// Numeric port from a host authority (`host:port`), else the scheme default.
fn host_port(host: &str, https: bool) -> u16 {
    if let Some((_, port)) = host.rsplit_once(':') {
        if let Ok(p) = port.parse::<u16>() {
            return p;
        }
    }
    if https {
        443
    } else {
        80
    }
}

/// `TrustedDomainHelper::getDomainWithoutPort`.
fn domain_without_port(host: &str) -> String {
    if let Some((head, tail)) = host.rsplit_once(':') {
        if tail.chars().all(|c| c.is_ascii_digit()) {
            return head.to_string();
        }
    }
    host.to_string()
}

/// `Request::REGEX_LOCALHOST` — `^(127\.0\.0\.1|localhost|\[::1\])$`.
fn is_localhost(domain: &str) -> bool {
    matches!(domain, "127.0.0.1" | "localhost" | "[::1]")
}

/// `TrustedDomainHelper::isTrustedDomain(domainWithPort)` — wildcard match
/// where each `*` becomes `[-\.a-zA-Z0-9]*` (case-insensitive), checked
/// against both the domain and the domain-with-port.
pub fn is_trusted_domain(cfg: &NcConfig, domain_with_port: &str) -> bool {
    // overwritehost is always trusted.
    if cfg
        .overwritehost
        .as_deref()
        .map(|h| !h.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    let domain = domain_without_port(domain_with_port);

    // PHP: `getSystemValue('trusted_domains', [])` — an absent key behaves as
    // an empty array, and the localhost check runs regardless of the list.
    let trusted_list = cfg.trusted_domains.as_deref().unwrap_or(&[]);

    // Always allow access from localhost.
    if is_localhost(&domain) {
        return true;
    }
    // Reject malformed domains in any case.
    if domain.starts_with('-') || domain.contains("..") {
        return false;
    }
    // Match, allowing for * wildcards.
    for trusted in trusted_list {
        let pattern = trusted
            .split('*')
            .map(|seg| regex_lite::escape(seg))
            .collect::<Vec<_>>()
            .join(r"[-\.a-zA-Z0-9]*");
        let Ok(re) = regex_lite::Regex::new(&format!("(?i)^{pattern}$")) else {
            continue;
        };
        if re.is_match(&domain) || re.is_match(domain_with_port) {
            return true;
        }
    }
    false
}

/// `lib/base.php:872-912` — trusted-domain enforcement, run once per request
/// after identity resolution (assets served by `try_static_files` bypass it,
/// matching the webserver-vs-PHP split of the canonical stack).
///
/// Returns `Some(400)` when the host is untrusted and the instance is
/// installed; `/status.php` gets the exact JSON body, `/css/*` pathinfo is
/// exempt, everything else gets a minimal error page (deviation: PHP renders
/// the themed `core/untrustedDomain` guest template — status, gating, and
/// exemptions identical, body chrome differs).
pub fn trusted_domains_response(
    cfg: &NcConfig,
    peer: IpAddr,
    headers: &HeaderMap,
    uri_path_and_query: &str,
    path_info: &str,
) -> Option<Response<axum::body::Body>> {
    if !cfg.installed {
        return None;
    }
    let host = insecure_server_host(peer, headers, cfg);
    if is_trusted_domain(cfg, &host) {
        return None;
    }
    // Allow access to CSS resources.
    if path_info.starts_with("/css/") {
        return None;
    }
    // PHP: `substr($request->getRequestUri(), -11) === '/status.php'` — the
    // full URI (path + query) ending check.
    if uri_path_and_query.ends_with("/status.php") {
        return Some(
            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"error": "Trusted domain error.", "code": 15}"#,
                ))
                .expect("trusted-domain JSON response is well-formed"),
        );
    }
    tracing::info!(
        remote_address = %peer,
        host = %host,
        "Trusted domain error. \"{peer}\" tried to access using \"{host}\" as host."
    );
    Some(
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("Content-Type", "text/html; charset=UTF-8")
            .body(axum::body::Body::from(
                "<html><body><h1>Bad Request</h1><p>Your are not allowed to access this server. \
                 See the <a href=\"https://docs.nextcloud.com/server/stable/admin_manual/installation/\
                 trusted_domains.html\">documentation</a>.</p></body></html>",
            ))
            .expect("trusted-domain page response is well-formed"),
    )
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn cfg(trusted_proxies: &[&str]) -> NcConfig {
        let mut c: NcConfig = serde_json::from_str("{}").expect("empty config");
        c.trusted_proxies = Some(trusted_proxies.iter().map(|s| s.to_string()).collect());
        c
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                k.parse::<axum::http::header::HeaderName>()
                    .expect("test header name"),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

    fn xff(pairs: &[(&str, &str)], c: &NcConfig) -> IpAddr {
        remote_address(PEER, &headers(pairs), c)
    }

    #[test]
    fn peer_addr_used_when_no_trusted_proxies_configured() {
        let c = cfg(&[]);
        let h = headers(&[("x-forwarded-for", "203.0.113.9")]);
        assert_eq!(remote_address(PEER, &h, &c), PEER);
        assert!(!is_trusted_proxy(&c, &PEER));
    }

    #[test]
    fn peer_addr_used_when_peer_not_trusted() {
        let c = cfg(&["10.0.0.2"]);
        let h = headers(&[("x-forwarded-for", "203.0.113.9")]);
        assert_eq!(remote_address(PEER, &h, &c), PEER);
    }

    #[test]
    fn single_proxy_returns_client_ip() {
        let c = cfg(&["10.0.0.1"]);
        assert_eq!(
            xff(&[("x-forwarded-for", "203.0.113.9")], &c),
            "203.0.113.9".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn chained_proxies_skip_trusted_right_to_left() {
        let c = cfg(&["10.0.0.1", "10.0.0.2"]);
        let h = headers(&[("x-forwarded-for", "203.0.113.9, 10.0.0.2")]);
        assert_eq!(
            remote_address(PEER, &h, &c),
            "203.0.113.9".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn untrusted_entry_stops_the_walk() {
        // A spoofed leftmost entry must be ignored: the rightmost untrusted
        // entry wins; entries left of it are never consulted.
        let c = cfg(&["10.0.0.1"]);
        let h = headers(&[("x-forwarded-for", "198.51.100.7, 203.0.113.9")]);
        assert_eq!(
            remote_address(PEER, &h, &c),
            "203.0.113.9".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn ipv4_with_port_stripped() {
        let c = cfg(&["10.0.0.1"]);
        assert_eq!(
            xff(&[("x-forwarded-for", "203.0.113.9:443")], &c),
            "203.0.113.9".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn ipv6_bracketed_with_port_stripped() {
        let c = cfg(&["10.0.0.1"]);
        let h = headers(&[("x-forwarded-for", "[2001:db8::1]:443")]);
        assert_eq!(
            remote_address(PEER, &h, &c),
            "2001:db8::1".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn malformed_entry_skipped() {
        let c = cfg(&["10.0.0.1"]);
        let h = headers(&[("x-forwarded-for", "not-an-ip, 203.0.113.9")]);
        assert_eq!(
            remote_address(PEER, &h, &c),
            "203.0.113.9".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn cidr_range_matches() {
        let c = cfg(&["10.0.0.0/8"]);
        let h = headers(&[("x-forwarded-for", "203.0.113.9")]);
        assert_eq!(
            remote_address(PEER, &h, &c),
            "203.0.113.9".parse::<std::net::IpAddr>().unwrap()
        );
        // peer inside the CIDR is trusted
        assert!(is_trusted_proxy(&c, &PEER));
        let outside: IpAddr = "11.0.0.1".parse::<std::net::IpAddr>().unwrap();
        assert!(!is_trusted_proxy(&c, &outside));
    }

    #[test]
    fn malformed_config_entry_never_matches() {
        // A malformed entry must never match — even when it is the only one.
        let c = cfg(&["not-a-cidr/"]);
        assert!(!is_trusted_proxy(&c, &PEER));
        // And a valid entry still matches next to a malformed one.
        let c2 = cfg(&["not-a-cidr/", "10.0.0.1"]);
        assert!(is_trusted_proxy(&c2, &PEER));
    }

    #[test]
    fn forwarded_host_ignored_from_untrusted_peer() {
        let c = cfg(&[]);
        let h = headers(&[("x-forwarded-host", "evil.example.com")]);
        assert_eq!(insecure_server_host(PEER, &h, &c), "localhost");
    }

    #[test]
    fn forwarded_host_used_from_trusted_peer() {
        let c = cfg(&["10.0.0.1"]);
        let h = headers(&[("x-forwarded-host", "files.example.com")]);
        assert_eq!(insecure_server_host(PEER, &h, &c), "files.example.com");
    }

    #[test]
    fn overwritehost_applies_only_from_trusted_proxy() {
        let mut c = cfg(&["10.0.0.1"]);
        c.overwritehost = Some("override.example.com".to_string());
        // untrusted peer: Host header wins
        let h = headers(&[("host", "plain.example.com")]);
        let untrusted: IpAddr = "198.51.100.9".parse().unwrap();
        assert_eq!(insecure_server_host(untrusted, &h, &c), "plain.example.com");
        // trusted peer: overwritehost wins
        assert_eq!(insecure_server_host(PEER, &h, &c), "override.example.com");
    }

    #[test]
    fn proto_from_xff_only_when_trusted() {
        let c = cfg(&["10.0.0.1"]);
        let h = headers(&[("x-forwarded-proto", "https")]);
        assert!(server_protocol(PEER, &h, &c));
        let untrusted: IpAddr = "198.51.100.9".parse().unwrap();
        assert!(!server_protocol(untrusted, &h, &c));
    }

    #[test]
    fn proto_comma_list_takes_first() {
        let c = cfg(&["10.0.0.1"]);
        let h = headers(&[("x-forwarded-proto", "https, http")]);
        assert!(server_protocol(PEER, &h, &c));
    }

    #[test]
    fn overwriteprotocol_gated_by_condaddr() {
        let mut c = cfg(&["10.0.0.1"]);
        c.overwriteprotocol = Some("https".to_string());
        c.overwritecondaddr = Some("10\\.0\\.0\\..*".to_string());
        let h = headers(&[]);
        assert!(server_protocol(PEER, &h, &c));
        let other: IpAddr = "192.168.0.1".parse().unwrap();
        assert!(!server_protocol(other, &h, &c));
    }

    #[test]
    fn invalid_proto_falls_back_to_http() {
        let c = cfg(&["10.0.0.1"]);
        let h = headers(&[("x-forwarded-proto", "ftp")]);
        assert!(!server_protocol(PEER, &h, &c));
    }

    #[test]
    fn untrusted_host_status_php_returns_json_400() {
        let mut c = cfg(&[]);
        c.installed = true;
        let h = headers(&[("host", "evil.example.com")]);
        let resp = trusted_domains_response(&c, PEER, &h, "/status.php", "").unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.headers()["content-type"], "application/json");
    }

    #[test]
    fn untrusted_host_css_pathinfo_passes_through() {
        let mut c = cfg(&[]);
        c.installed = true;
        let h = headers(&[("host", "evil.example.com")]);
        assert!(trusted_domains_response(&c, PEER, &h, "/index.php/css/x", "/css/x").is_none());
    }

    #[test]
    fn untrusted_host_other_returns_400_page() {
        let mut c = cfg(&[]);
        c.installed = true;
        let h = headers(&[("host", "evil.example.com")]);
        let resp = trusted_domains_response(&c, PEER, &h, "/index.php/apps/files/", "").unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn untrusted_check_skipped_when_not_installed() {
        let mut c = cfg(&[]);
        c.installed = false;
        let h = headers(&[("host", "evil.example.com")]);
        assert!(trusted_domains_response(&c, PEER, &h, "/index.php", "").is_none());
    }

    #[test]
    fn wildcard_trusted_domain_matches() {
        let mut c = cfg(&[]);
        c.trusted_domains = Some(vec!["*.example.com".to_string()]);
        assert!(is_trusted_domain(&c, "files.example.com"));
        assert!(!is_trusted_domain(&c, "files.example.org"));
        // domain-with-port also matches
        assert!(is_trusted_domain(&c, "files.example.com:8443"));
    }

    #[test]
    fn localhost_always_trusted() {
        let c = cfg(&[]);
        assert!(is_trusted_domain(&c, "localhost"));
        assert!(is_trusted_domain(&c, "127.0.0.1"));
        assert!(is_trusted_domain(&c, "[::1]"));
    }
}
