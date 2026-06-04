/// HTTP/1.1 request types.
///
/// `Request` is the fully-parsed, owned representation produced by
/// `parser::parse_request_head` once the header block is complete.
/// The body bytes (if any) are appended separately after the parser
/// confirms the full body has arrived.
use std::collections::HashMap;

// Re-export Method from config so the whole codebase uses one definition.
pub use crate::config::types::Method;

// ---------------------------------------------------------------------------
// HTTP Version
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Version {
    Http10,
    Http11,
}

impl Version {
    pub fn as_str(&self) -> &'static str {
        match self {
            Version::Http10 => "HTTP/1.0",
            Version::Http11 => "HTTP/1.1",
        }
    }

    /// Returns `true` for HTTP/1.1, where keep-alive is the default.
    pub fn keep_alive_default(&self) -> bool {
        matches!(self, Version::Http11)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// HeaderMap
// ---------------------------------------------------------------------------

/// Case-insensitive HTTP header store.
///
/// Header names are normalised to lowercase on insertion so lookups are
/// always case-insensitive without extra allocations.
///
/// Multiple values for the same header are joined with `", "` per RFC 9110
/// §5.3 (field-value combination rule). This is correct for most headers;
/// `Set-Cookie` is the notable exception, but we never produce multiple
/// `Set-Cookie` headers in a request context.
#[derive(Debug, Clone, Default)]
pub struct HeaderMap {
    inner: HashMap<String, String>,
}

impl HeaderMap {
    pub fn new() -> Self {
        HeaderMap { inner: HashMap::new() }
    }

    /// Insert a header, combining with existing value if present.
    pub fn insert(&mut self, name: &str, value: &str) {
        let key = name.to_lowercase();
        let val = value.trim().to_string();
        self.inner
            .entry(key)
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(&val);
            })
            .or_insert(val);
    }

    /// Look up a header by name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.inner.get(&name.to_lowercase()).map(String::as_str)
    }

    /// Returns `true` if the header is present.
    pub fn contains(&self, name: &str) -> bool {
        self.inner.contains_key(&name.to_lowercase())
    }

    /// Iterate over all (name, value) pairs.
    /// Names are already lowercase.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.inner.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// A fully parsed HTTP/1.1 request.
#[derive(Debug, Clone)]
pub struct Request {
    /// HTTP method (GET, POST, DELETE, …).
    pub method: Method,

    /// Decoded request path, without query string (e.g. `/static/img/logo.png`).
    pub path: String,

    /// Raw query string, without the leading `?` (empty string if absent).
    pub query: String,

    /// Protocol version.
    pub version: Version,

    /// All parsed request headers.
    pub headers: HeaderMap,

    /// Request body bytes. Empty for methods that carry no body (GET, HEAD).
    pub body: Vec<u8>,
}

impl Request {
    /// Convenience: return the `Host` header value, stripped of any port.
    pub fn host(&self) -> &str {
        self.headers
            .get("host")
            .map(|h| h.split(':').next().unwrap_or(h))
            .unwrap_or("")
    }

    /// Convenience: `Content-Length` as a `usize`, or `None`.
    pub fn content_length(&self) -> Option<usize> {
        self.headers
            .get("content-length")
            .and_then(|v| v.trim().parse().ok())
    }

    /// Returns `true` if the client wants a persistent connection.
    ///
    /// Rules per RFC 9110 §9.3 / RFC 9112 §9.3:
    /// - HTTP/1.1: keep-alive unless `Connection: close` is present.
    /// - HTTP/1.0: close unless `Connection: keep-alive` is present.
    pub fn is_keep_alive(&self) -> bool {
        match self.headers.get("connection") {
            Some(v) => {
                let v = v.to_lowercase();
                if v.contains("close") {
                    false
                } else if v.contains("keep-alive") {
                    true
                } else {
                    self.version.keep_alive_default()
                }
            }
            None => self.version.keep_alive_default(),
        }
    }

    /// Returns `true` if the transfer encoding is chunked.
    pub fn is_chunked(&self) -> bool {
        self.headers
            .get("transfer-encoding")
            .map(|v| v.to_lowercase().contains("chunked"))
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// ParseError
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The request line is malformed (wrong number of tokens, bad version…).
    BadRequestLine(String),
    /// A header line could not be parsed.
    BadHeader(String),
    /// The HTTP method is not recognised.
    UnknownMethod(String),
    /// The HTTP version string is not `HTTP/1.0` or `HTTP/1.1`.
    UnsupportedVersion(String),
    /// The URI is empty or otherwise invalid.
    BadUri(String),
    /// Headers exceed the maximum allowed size.
    HeadersTooLarge,
    /// The `Content-Length` value is not a valid non-negative integer.
    BadContentLength(String),
    /// A chunked body chunk size line is malformed.
    BadChunkSize(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::BadRequestLine(s)      => write!(f, "bad request line: {s}"),
            ParseError::BadHeader(s)           => write!(f, "bad header: {s}"),
            ParseError::UnknownMethod(s)       => write!(f, "unknown method: {s}"),
            ParseError::UnsupportedVersion(s)  => write!(f, "unsupported version: {s}"),
            ParseError::BadUri(s)              => write!(f, "bad URI: {s}"),
            ParseError::HeadersTooLarge        => write!(f, "request headers too large"),
            ParseError::BadContentLength(s)    => write!(f, "bad Content-Length: {s}"),
            ParseError::BadChunkSize(s)        => write!(f, "bad chunk size: {s}"),
        }
    }
}

impl std::error::Error for ParseError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Method;

    fn minimal_request() -> Request {
        Request {
            method:  Method::Get,
            path:    "/".into(),
            query:   String::new(),
            version: Version::Http11,
            headers: HeaderMap::new(),
            body:    Vec::new(),
        }
    }

    #[test]
    fn version_display() {
        assert_eq!(Version::Http10.as_str(), "HTTP/1.0");
        assert_eq!(Version::Http11.as_str(), "HTTP/1.1");
    }

    #[test]
    fn http11_keep_alive_by_default() {
        let req = minimal_request();
        assert!(req.is_keep_alive());
    }

    #[test]
    fn http11_connection_close_disables_keep_alive() {
        let mut req = minimal_request();
        req.headers.insert("Connection", "close");
        assert!(!req.is_keep_alive());
    }

    #[test]
    fn http10_close_by_default() {
        let mut req = minimal_request();
        req.version = Version::Http10;
        assert!(!req.is_keep_alive());
    }

    #[test]
    fn http10_keep_alive_header_enables_keep_alive() {
        let mut req = minimal_request();
        req.version = Version::Http10;
        req.headers.insert("Connection", "keep-alive");
        assert!(req.is_keep_alive());
    }

    #[test]
    fn header_map_case_insensitive() {
        let mut map = HeaderMap::new();
        map.insert("Content-Type", "text/html");
        assert_eq!(map.get("content-type"), Some("text/html"));
        assert_eq!(map.get("CONTENT-TYPE"), Some("text/html"));
        assert!(map.contains("Content-Type"));
    }

    #[test]
    fn header_map_multi_value_combined() {
        let mut map = HeaderMap::new();
        map.insert("Accept", "text/html");
        map.insert("Accept", "application/json");
        assert_eq!(map.get("accept"), Some("text/html, application/json"));
    }

    #[test]
    fn content_length_parsed() {
        let mut req = minimal_request();
        req.headers.insert("Content-Length", "42");
        assert_eq!(req.content_length(), Some(42));
    }

    #[test]
    fn content_length_missing_returns_none() {
        let req = minimal_request();
        assert_eq!(req.content_length(), None);
    }

    #[test]
    fn host_strips_port() {
        let mut req = minimal_request();
        req.headers.insert("Host", "example.com:8080");
        assert_eq!(req.host(), "example.com");
    }

    #[test]
    fn is_chunked_detects_transfer_encoding() {
        let mut req = minimal_request();
        req.headers.insert("Transfer-Encoding", "chunked");
        assert!(req.is_chunked());
    }
}