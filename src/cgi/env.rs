//! CGI/1.1 environment variable construction.
//!
//! Builds the `(name, value)` pairs handed to the child via `execve`'s
//! `envp`. We follow CGI/1.1 (RFC 3875) for the standard meta-variables and
//! prefix every request header with `HTTP_` per §4.1.18.
//!
//! Two project-specific notes:
//! - `PATH_INFO` is set to the absolute script path. The exercise states the
//!   CGI "will check `PATH_INFO` to define the full path", so we point it at
//!   the script itself (alongside the conventional `SCRIPT_FILENAME`).
//! - `REDIRECT_STATUS=200` is set unconditionally: `php-cgi` refuses to run
//!   without it, and it is harmless to other interpreters (Python, etc.).
use std::net::SocketAddr;
use std::path::Path;

use crate::config::types::ServerConfig;
use crate::http::request::types::Request;

/// Build the full CGI environment for one request.
///
/// `script_path` is the absolute path to the script file; `path_info` is the
/// value to expose as `PATH_INFO` (the dispatcher passes the script path).
pub fn build_env(
    req: &Request,
    script_path: &Path,
    path_info: &Path,
    server: &ServerConfig,
    server_port: u16,
    peer: SocketAddr,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::with_capacity(24);

    let script = script_path.to_string_lossy().into_owned();

    // --- CGI/1.1 core meta-variables ---
    env.push(("GATEWAY_INTERFACE".into(), "CGI/1.1".into()));
    env.push(("SERVER_PROTOCOL".into(), "HTTP/1.1".into()));
    env.push(("SERVER_SOFTWARE".into(), "localhost/0.1".into()));
    env.push(("REDIRECT_STATUS".into(), "200".into()));

    env.push(("REQUEST_METHOD".into(), req.method.as_str().into()));
    env.push(("SCRIPT_NAME".into(), req.path.clone()));
    env.push(("SCRIPT_FILENAME".into(), script.clone()));
    env.push(("PATH_INFO".into(), path_info.to_string_lossy().into_owned()));
    env.push(("PATH_TRANSLATED".into(), script));
    env.push(("QUERY_STRING".into(), req.query.clone()));
    env.push(("REQUEST_URI".into(), request_uri(req)));

    // --- body framing ---
    // We always feed the body via stdin, so CONTENT_LENGTH is authoritative.
    env.push(("CONTENT_LENGTH".into(), req.body.len().to_string()));
    if let Some(ct) = req.headers.get("content-type") {
        env.push(("CONTENT_TYPE".into(), ct.to_string()));
    }

    // --- server / peer identity ---
    let server_name = server
        .server_names
        .first()
        .map(String::as_str)
        .unwrap_or(server.host.as_str());
    env.push(("SERVER_NAME".into(), server_name.to_string()));
    env.push(("SERVER_PORT".into(), server_port.to_string()));
    env.push(("REMOTE_ADDR".into(), peer.ip().to_string()));
    env.push(("REMOTE_PORT".into(), peer.port().to_string()));

    // --- request headers as HTTP_* ---
    // Content-Type / Content-Length are exposed via their dedicated vars only.
    for (name, value) in req.headers.iter() {
        if name == "content-type" || name == "content-length" {
            continue;
        }
        let key = format!("HTTP_{}", name.to_uppercase().replace('-', "_"));
        env.push((key, value.to_string()));
    }

    env
}

/// Reconstruct the request-target (`path` plus `?query` when present).
fn request_uri(req: &Request) -> String {
    if req.query.is_empty() {
        req.path.clone()
    } else {
        format!("{}?{}", req.path, req.query)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ServerConfig;
    use crate::http::request::types::{HeaderMap, Method, Request, Version};
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::Path;

    fn sample_request() -> Request {
        let mut h = HeaderMap::new();
        h.insert("Host", "example.com");
        h.insert("Content-Type", "application/x-www-form-urlencoded");
        h.insert("User-Agent", "test-agent");
        Request {
            method: Method::Post,
            path: "/cgi-bin/hi.py".into(),
            query: "a=1&b=2".into(),
            version: Version::Http11,
            headers: h,
            body: b"x=42".to_vec(),
        }
    }

    fn env_map(env: Vec<(String, String)>) -> HashMap<String, String> {
        env.into_iter().collect()
    }

    fn peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 55555)
    }

    #[test]
    fn sets_core_meta_variables() {
        let m = env_map(build_env(
            &sample_request(),
            Path::new("/srv/hi.py"),
            Path::new("/srv/hi.py"),
            &ServerConfig::default(),
            8080,
            peer(),
        ));

        assert_eq!(m.get("GATEWAY_INTERFACE").map(String::as_str), Some("CGI/1.1"));
        assert_eq!(m.get("REQUEST_METHOD").map(String::as_str), Some("POST"));
        assert_eq!(m.get("QUERY_STRING").map(String::as_str), Some("a=1&b=2"));
        assert_eq!(m.get("CONTENT_LENGTH").map(String::as_str), Some("4"));
        assert_eq!(
            m.get("CONTENT_TYPE").map(String::as_str),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(m.get("SCRIPT_FILENAME").map(String::as_str), Some("/srv/hi.py"));
        assert_eq!(m.get("PATH_INFO").map(String::as_str), Some("/srv/hi.py"));
        assert_eq!(m.get("SERVER_PORT").map(String::as_str), Some("8080"));
        assert_eq!(m.get("REQUEST_URI").map(String::as_str), Some("/cgi-bin/hi.py?a=1&b=2"));
        assert_eq!(m.get("REDIRECT_STATUS").map(String::as_str), Some("200"));
    }

    #[test]
    fn maps_headers_to_http_prefix() {
        let m = env_map(build_env(
            &sample_request(),
            Path::new("/srv/hi.py"),
            Path::new("/srv/hi.py"),
            &ServerConfig::default(),
            8080,
            peer(),
        ));

        assert_eq!(m.get("HTTP_HOST").map(String::as_str), Some("example.com"));
        assert_eq!(m.get("HTTP_USER_AGENT").map(String::as_str), Some("test-agent"));
        // Content-Type must NOT be duplicated under HTTP_*.
        assert!(!m.contains_key("HTTP_CONTENT_TYPE"));
        assert!(!m.contains_key("HTTP_CONTENT_LENGTH"));
    }

    #[test]
    fn empty_query_yields_bare_request_uri() {
        let mut req = sample_request();
        req.query = String::new();
        let m = env_map(build_env(
            &req,
            Path::new("/srv/hi.py"),
            Path::new("/srv/hi.py"),
            &ServerConfig::default(),
            8080,
            peer(),
        ));
        assert_eq!(m.get("REQUEST_URI").map(String::as_str), Some("/cgi-bin/hi.py"));
        assert_eq!(m.get("QUERY_STRING").map(String::as_str), Some(""));
    }
}