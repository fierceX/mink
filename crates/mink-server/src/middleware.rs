//! HTTP host, origin, and security header guard for mink-server.

use crate::api::ApiResponse;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, HOST, ORIGIN, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderName, HeaderValue, Method, Request};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use std::net::IpAddr;
use std::sync::Arc;

const HEADER_REFERRER_POLICY: HeaderName = HeaderName::from_static("referrer-policy");
const HEADER_X_FRAME_OPTIONS: HeaderName = HeaderName::from_static("x-frame-options");
const HEADER_SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");

/// Host/Origin policy derived from the listening address and `MINK_SERVER_ALLOWED_HOSTS`.
#[derive(Debug, Clone)]
pub struct HostGuardConfig {
    pub loopback_only: bool,
    pub allowed_hosts: Vec<String>,
}

impl HostGuardConfig {
    #[cfg(test)]
    pub fn loopback_default() -> Self {
        Self {
            loopback_only: true,
            allowed_hosts: Vec::new(),
        }
    }

    pub fn from_bind_host(host: &str, allowed_hosts_env: Option<&str>) -> Self {
        let loopback_only = host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
        let allowed_hosts = allowed_hosts_env
            .unwrap_or_default()
            .split(',')
            .filter_map(normalize_configured_host)
            .collect();
        Self {
            loopback_only,
            allowed_hosts,
        }
    }

    fn allows_host(&self, authority: &str, hostname: &str) -> bool {
        if self.allowed_hosts.is_empty() {
            return !self.loopback_only || is_loopback_hostname(hostname);
        }
        if self.loopback_only && is_loopback_hostname(hostname) {
            return true;
        }
        self.allowed_hosts
            .iter()
            .any(|allowed| allowed == authority || allowed == hostname)
    }
}

fn normalize_configured_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('[')
        && let Some(end) = trimmed.find(']')
        && end + 1 == trimmed.len()
    {
        return Some(trimmed[1..end].to_string());
    }
    Some(trimmed)
}

fn is_loopback_hostname(hostname: &str) -> bool {
    matches!(hostname, "localhost" | "127.0.0.1" | "::1")
}

/// Normalize an HTTP `Host` / authority value to `(authority_lower, hostname_lower)`.
fn parse_authority(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('@')
        || trimmed.chars().any(char::is_whitespace)
    {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &rest[..close];
        if host.is_empty() {
            return None;
        }
        let suffix = &rest[close + 1..];
        if !suffix.is_empty() {
            let port = suffix.strip_prefix(':')?;
            if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
        }
        return Some((trimmed.clone(), host.to_string()));
    }
    if let Some((host, port)) = trimmed.rsplit_once(':') {
        if host.contains(':') {
            // Unbracketed IPv6 literal without port
            return Some((trimmed.clone(), trimmed));
        }
        if host.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        return Some((trimmed.clone(), host.to_string()));
    }
    Some((trimmed.clone(), trimmed))
}

fn parse_origin_authority(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))?;
    if rest.is_empty() {
        return None;
    }
    let (authority, _) = parse_authority(rest)?;
    Some(authority)
}

fn is_mutating_method(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn insert_default_header(
    headers: &mut axum::http::HeaderMap,
    name: HeaderName,
    value: &'static str,
) {
    if !headers.contains_key(&name) {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

pub fn apply_security_headers(headers: &mut axum::http::HeaderMap, path: &str) {
    insert_default_header(headers, X_CONTENT_TYPE_OPTIONS, "nosniff");
    insert_default_header(headers, HEADER_REFERRER_POLICY, "no-referrer");
    insert_default_header(headers, HEADER_X_FRAME_OPTIONS, "DENY");
    if path.starts_with("/api/") || path == "/health" {
        insert_default_header(headers, CACHE_CONTROL, "no-store");
    }
}

pub async fn security_middleware(
    State(guard): State<Arc<HostGuardConfig>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    let parsed_host = match req.headers().get(HOST) {
        Some(raw) => match raw.to_str().ok().and_then(parse_authority) {
            Some(parsed) => {
                if !guard.allows_host(&parsed.0, &parsed.1) {
                    let mut res = ApiResponse::err(403, "forbidden host").into_response();
                    apply_security_headers(res.headers_mut(), &path);
                    return res;
                }
                Some(parsed)
            }
            None => {
                let mut res = ApiResponse::err(403, "invalid host header").into_response();
                apply_security_headers(res.headers_mut(), &path);
                return res;
            }
        },
        None => None,
    };

    if is_mutating_method(req.method()) {
        if let Some(fetch_site) = req
            .headers()
            .get(&HEADER_SEC_FETCH_SITE)
            .and_then(|v| v.to_str().ok())
            && fetch_site.trim().eq_ignore_ascii_case("cross-site")
        {
            let mut res = ApiResponse::err(403, "cross-site request forbidden").into_response();
            apply_security_headers(res.headers_mut(), &path);
            return res;
        }
        if let Some(origin_raw) = req.headers().get(ORIGIN) {
            let valid = origin_raw
                .to_str()
                .ok()
                .and_then(parse_origin_authority)
                .is_some_and(|origin_auth| {
                    parsed_host
                        .as_ref()
                        .is_some_and(|(host_auth, _)| origin_auth == *host_auth)
                });
            if !valid {
                let mut res =
                    ApiResponse::err(403, "cross-origin request forbidden").into_response();
                apply_security_headers(res.headers_mut(), &path);
                return res;
            }
        }
    }

    let mut res = next.run(req).await;
    apply_security_headers(res.headers_mut(), &path);
    res
}

pub fn apply_security_layer(router: axum::Router, guard: Arc<HostGuardConfig>) -> axum::Router {
    router.layer(middleware::from_fn_with_state(guard, security_middleware))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_authority_supports_ipv4_hostname_and_bracketed_ipv6() {
        assert_eq!(
            parse_authority("LocalHost:8765"),
            Some(("localhost:8765".into(), "localhost".into()))
        );
        assert_eq!(
            parse_authority("127.0.0.1"),
            Some(("127.0.0.1".into(), "127.0.0.1".into()))
        );
        assert_eq!(
            parse_authority("[::1]:8765"),
            Some(("[::1]:8765".into(), "::1".into()))
        );
        assert_eq!(parse_authority("evil.com/path"), None);
        assert_eq!(parse_authority("localhost:notaport"), None);
    }

    #[test]
    fn loopback_guard_blocks_rebinding_hosts_unless_explicitly_allowed() {
        let guard = HostGuardConfig::loopback_default();
        assert!(guard.allows_host("127.0.0.1:8765", "127.0.0.1"));
        assert!(guard.allows_host("localhost:5173", "localhost"));
        assert!(guard.allows_host("[::1]:8765", "::1"));
        assert!(!guard.allows_host("evil.example:8765", "evil.example"));

        let with_allowlist = HostGuardConfig::from_bind_host("127.0.0.1", Some("dev.internal"));
        assert!(with_allowlist.allows_host("dev.internal:8765", "dev.internal"));
        assert!(with_allowlist.allows_host("localhost:8765", "localhost"));
        assert!(!with_allowlist.allows_host("evil.example:8765", "evil.example"));
    }

    #[test]
    fn origin_parser_accepts_http_https_and_rejects_null_or_paths() {
        assert_eq!(
            parse_origin_authority("http://localhost:5173"),
            Some("localhost:5173".into())
        );
        assert_eq!(
            parse_origin_authority("https://[::1]:8765"),
            Some("[::1]:8765".into())
        );
        assert_eq!(parse_origin_authority("null"), None);
        assert_eq!(parse_origin_authority("http://localhost:5173/extra"), None);
    }
}
