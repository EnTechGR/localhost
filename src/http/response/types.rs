/// HTTP response types.
///
/// `Response` is the owned representation produced by `builder.rs` and
/// consumed by `writer.rs`. The `HeaderMap` here is write-oriented:
/// insertion order is preserved for deterministic wire output.

// ---------------------------------------------------------------------------
// StatusCode
// ---------------------------------------------------------------------------

/// HTTP status codes used by this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusCode(pub u16);

#[allow(dead_code)]
impl StatusCode {
    /// All constants are part of the public API surface for consumers to
    /// compare against status codes returned by handlers, and internally they
    /// replace raw integer literals throughout the codebase.
    pub const OK:                    StatusCode = StatusCode(200);
    pub const CREATED:               StatusCode = StatusCode(201);
    pub const NO_CONTENT:            StatusCode = StatusCode(204);
    // 3xx
    pub const MOVED_PERMANENTLY:     StatusCode = StatusCode(301);
    pub const FOUND:                 StatusCode = StatusCode(302);
    pub const TEMPORARY_REDIRECT:    StatusCode = StatusCode(307);
    pub const PERMANENT_REDIRECT:    StatusCode = StatusCode(308);
    // 4xx
    pub const BAD_REQUEST:           StatusCode = StatusCode(400);
    pub const FORBIDDEN:             StatusCode = StatusCode(403);
    pub const NOT_FOUND:             StatusCode = StatusCode(404);
    pub const METHOD_NOT_ALLOWED:    StatusCode = StatusCode(405);
    pub const CONFLICT:              StatusCode = StatusCode(409);
    pub const REQUEST_TIMEOUT:       StatusCode = StatusCode(408);
    pub const PAYLOAD_TOO_LARGE:     StatusCode = StatusCode(413);
    pub const URI_TOO_LONG:          StatusCode = StatusCode(414);
    // 5xx
    pub const INTERNAL_SERVER_ERROR: StatusCode = StatusCode(500);
    pub const BAD_GATEWAY:           StatusCode = StatusCode(502);
    pub const GATEWAY_TIMEOUT:       StatusCode = StatusCode(504);

    /// Extract the raw u16 status code value.
    pub fn code(self) -> u16 { self.0 }

    /// Standard reason phrase for well-known codes.
    pub fn reason(self) -> &'static str {
        match self.0 {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            301 => "Moved Permanently",
            302 => "Found",
            307 => "Temporary Redirect",
            308 => "Permanent Redirect",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            408 => "Request Timeout",
            413 => "Payload Too Large",
            414 => "URI Too Long",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            504 => "Gateway Timeout",
            _   => "Unknown",
        }
    }

    /// Return true for 1xx / 204 / 304 — responses that must not include a body.
    pub fn must_not_have_body(self) -> bool {
        matches!(self.0, 100..=199 | 204 | 304)
    }
}

impl std::fmt::Display for StatusCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.0, self.reason())
    }
}

// ---------------------------------------------------------------------------
// ResponseHeaderMap
// ---------------------------------------------------------------------------

/// Ordered response header store.
///
/// Unlike the request `HeaderMap`, we preserve insertion order for
/// deterministic wire output and allow multiple values per name
/// (needed for `Set-Cookie`).
#[derive(Debug, Clone, Default)]
pub struct ResponseHeaders {
    // Vec preserves insertion order; names are stored in their original case.
    entries: Vec<(String, String)>,
}

impl ResponseHeaders {
    pub fn new() -> Self {
        ResponseHeaders { entries: Vec::new() }
    }

    /// Append a header entry. Multiple values for the same name are permitted.
    pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries.push((name.into(), value.into()));
    }

    /// Set a header, replacing any existing entry with the same name
    /// (case-insensitive). Retains the original capitalisation of `name`.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name  = name.into();
        let value = value.into();
        let lower = name.to_lowercase();
        // Remove all existing entries with this name.
        self.entries.retain(|(k, _)| k.to_lowercase() != lower);
        self.entries.push((name, value));
    }

    /// Return the value of the first matching header, if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        let lower = name.to_lowercase();
        self.entries
            .iter()
            .find(|(k, _)| k.to_lowercase() == lower)
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// A fully constructed HTTP/1.1 response, ready for serialisation.
#[derive(Debug, Clone)]
pub struct Response {
    pub status:  StatusCode,
    pub headers: ResponseHeaders,
    pub body:    Vec<u8>,
}

impl Response {
    pub fn new(status: StatusCode) -> Self {
        Response {
            status,
            headers: ResponseHeaders::new(),
            body:    Vec::new(),
        }
    }

}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_code_reason_phrases() {
        assert_eq!(StatusCode::OK.reason(),                    "OK");
        assert_eq!(StatusCode::NOT_FOUND.reason(),             "Not Found");
        assert_eq!(StatusCode::INTERNAL_SERVER_ERROR.reason(), "Internal Server Error");
        assert_eq!(StatusCode(999).reason(),                   "Unknown");
    }

    #[test]
    fn status_display() {
        assert_eq!(StatusCode::OK.to_string(),       "200 OK");
        assert_eq!(StatusCode::NOT_FOUND.to_string(), "404 Not Found");
    }

    #[test]
    fn must_not_have_body_codes() {
        assert!(StatusCode::NO_CONTENT.must_not_have_body());
        assert!(!StatusCode::OK.must_not_have_body());
        assert!(!StatusCode::NOT_FOUND.must_not_have_body());
    }

    #[test]
    fn response_headers_set_replaces() {
        let mut h = ResponseHeaders::new();
        h.set("Content-Type", "text/plain");
        h.set("Content-Type", "text/html");
        assert_eq!(h.get("Content-Type"), Some("text/html"));
        // Verify first entry was removed by checking the value changed.
        assert_eq!(h.iter().count(), 1);
    }

    #[test]
    fn response_headers_append_allows_duplicates() {
        let mut h = ResponseHeaders::new();
        h.append("Set-Cookie", "a=1");
        h.append("Set-Cookie", "b=2");
        assert_eq!(h.iter().count(), 2);
    }

    #[test]
    fn response_headers_case_insensitive_get() {
        let mut h = ResponseHeaders::new();
        h.set("Content-Length", "42");
        assert_eq!(h.get("content-length"), Some("42"));
        assert_eq!(h.get("CONTENT-LENGTH"), Some("42"));
    }

    #[test]
    fn response_can_be_constructed_with_body_directly() {
        let mut resp = Response::new(StatusCode::OK);
        resp.headers.set("Content-Type",   "text/plain");
        resp.headers.set("Content-Length", "5");
        resp.body = b"hello".to_vec();
        assert_eq!(resp.headers.get("Content-Length"), Some("5"));
        assert_eq!(resp.body, b"hello");
    }
}