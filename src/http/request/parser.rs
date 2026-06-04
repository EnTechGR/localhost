/// Incremental HTTP/1.1 request parser.
///
/// Designed to work with a growing byte buffer: call `parse_request_head`
/// each time new bytes arrive. It returns `Incomplete` until the full header
/// block (`\r\n\r\n`) is present, then parses and returns a `Request`.
///
/// # RFC compliance
///
/// - RFC 9112 §3   — request line format
/// - RFC 9112 §5   — header field syntax
/// - RFC 9110 §4   — HTTP method tokens (case-sensitive)
/// - RFC 3986 §3   — URI path / query splitting
///
/// # Limits
///
/// `MAX_HEADER_BYTES` guards against header-flooding attacks. Individual
/// header count is not capped at the parser level; the OS will close the
/// connection before an attacker can exhaust memory given the per-connection
/// read buffer cap in the dispatcher.
use crate::http::request::types::{HeaderMap, ParseError, Request, Version};
use crate::config::types::Method;

/// Maximum header block size (request line + headers up to `\r\n\r\n`).
/// Requests exceeding this get a 431 / 400 response.
pub const MAX_HEADER_BYTES: usize = 16_384; // 16 KiB

// ---------------------------------------------------------------------------
// ParseResult
// ---------------------------------------------------------------------------

/// Result of attempting to parse a request from an incomplete byte buffer.
#[derive(Debug)]
pub enum ParseResult {
    /// A complete, valid request was parsed.
    /// The associated `usize` is the number of bytes consumed from `buf`
    /// (header block including `\r\n\r\n` terminator). The caller should
    /// keep bytes beyond that offset in the read buffer as the start of the
    /// body (or the next pipelined request).
    Complete(Request, usize),

    /// The buffer does not yet contain a full header block. Accumulate more
    /// bytes and call again.
    Incomplete,

    /// The data is syntactically invalid. The caller should send a 400 and
    /// close the connection.
    Error(ParseError),
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Attempt to parse an HTTP/1.1 request from `buf`.
///
/// Returns `Incomplete` until `\r\n\r\n` is found.
/// The `body` field of the returned `Request` is always empty; the caller
/// is responsible for reading the body based on `Content-Length` /
/// `Transfer-Encoding` (see `body.rs`).
pub fn parse_request_head(buf: &[u8]) -> ParseResult {
    if buf.len() > MAX_HEADER_BYTES {
        return ParseResult::Error(ParseError::HeadersTooLarge);
    }

    // Locate the end-of-headers marker.
    let header_end = match find_header_end(buf) {
        Some(pos) => pos,
        None      => return ParseResult::Incomplete,
    };

    // `header_end` points to the first byte of \r\n\r\n, so the consumed
    // length includes those 4 bytes.
    let consumed  = header_end + 4;
    let head_bytes = &buf[..header_end];

    // Convert to &str — HTTP headers must be ASCII / ISO-8859-1.
    let head_str = match std::str::from_utf8(head_bytes) {
        Ok(s)  => s,
        Err(_) => return ParseResult::Error(
            ParseError::BadRequestLine("non-UTF-8 in request head".into())
        ),
    };

    // Split into lines, separating the request line from the headers.
    let mut lines = head_str.split("\r\n");

    let request_line = match lines.next() {
        Some(l) if !l.is_empty() => l,
        _ => return ParseResult::Error(ParseError::BadRequestLine("empty request line".into())),
    };

    let (method, path, query, version) = match parse_request_line(request_line) {
        Ok(t)  => t,
        Err(e) => return ParseResult::Error(e),
    };

    let header_lines: Vec<&str> = lines.collect();
    let headers = match parse_headers(&header_lines) {
        Ok(h)  => h,
        Err(e) => return ParseResult::Error(e),
    };

    let request = Request {
        method,
        path,
        query,
        version,
        headers,
        body: Vec::new(),
    };

    ParseResult::Complete(request, consumed)
}

// ---------------------------------------------------------------------------
// parse_request_line
// ---------------------------------------------------------------------------

/// Parse `"METHOD /path?query HTTP/1.1"` into its components.
pub fn parse_request_line(
    line: &str,
) -> Result<(Method, String, String, Version), ParseError> {
    // Exactly three whitespace-separated tokens.
    let mut parts = line.splitn(3, ' ');

    let method_str = parts.next().unwrap_or("");
    let uri        = parts.next().unwrap_or("");
    let version_str = parts.next().unwrap_or("");

    // Method
    let method = Method::from_str(method_str)
        .ok_or_else(|| ParseError::UnknownMethod(method_str.to_string()))?;

    // URI: split on '?' to separate path from query string.
    if uri.is_empty() {
        return Err(ParseError::BadUri("empty URI".into()));
    }
    let (raw_path, query) = match uri.find('?') {
        Some(pos) => (&uri[..pos], uri[pos + 1..].to_string()),
        None      => (uri, String::new()),
    };

    // Minimal URI validation: must start with '/' (for origin-form) or be '*'.
    if raw_path != "*" && !raw_path.starts_with('/') {
        return Err(ParseError::BadUri(format!("URI must start with '/': {uri}")));
    }

    // Percent-decode the path component.
    let path = percent_decode(raw_path);

    // Version
    let version = match version_str {
        "HTTP/1.1" => Version::Http11,
        "HTTP/1.0" => Version::Http10,
        other      => return Err(ParseError::UnsupportedVersion(other.to_string())),
    };

    Ok((method, path, query, version))
}

// ---------------------------------------------------------------------------
// parse_headers
// ---------------------------------------------------------------------------

/// Parse a slice of header lines (each `"Name: value"`) into a `HeaderMap`.
///
/// Empty lines (the blank line after the last header in some callers) are
/// skipped. Folded header lines (obsolete per RFC 7230 §3.2.4) are rejected.
pub fn parse_headers(lines: &[&str]) -> Result<HeaderMap, ParseError> {
    let mut map = HeaderMap::new();

    for line in lines {
        if line.is_empty() {
            continue;
        }

        // Reject obsolete line folding (SP / HTAB at start of line).
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(ParseError::BadHeader(format!(
                "obsolete header folding: {line}"
            )));
        }

        // Split on the first ':' only.
        let colon = line.find(':').ok_or_else(|| {
            ParseError::BadHeader(format!("missing ':' in header: {line}"))
        })?;

        let name  = &line[..colon];
        let value = &line[colon + 1..];

        // RFC 9110 §5.1: header name must be a non-empty token.
        if name.is_empty() {
            return Err(ParseError::BadHeader("empty header name".into()));
        }
        // RFC 9112 §5.1: no whitespace between field name and colon.
        if name.ends_with(' ') || name.ends_with('\t') {
            return Err(ParseError::BadHeader(format!(
                "whitespace before ':' in header '{name}'"
            )));
        }

        map.insert(name, value.trim());
    }

    Ok(map)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Find the index of the first `\r\n\r\n` in `buf`.
/// Returns the index of the leading `\r` of the terminator.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Percent-decode a URI path component.
///
/// Invalid `%XX` sequences (non-hex digits or incomplete sequence) are left
/// as-is rather than returning an error; this matches the behaviour of most
/// production servers and avoids easy 400 exploits via malformed encodings.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut i   = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                hex_nibble(bytes[i + 1]),
                hex_nibble(bytes[i + 2]),
            ) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }

    // SAFETY: we only decoded valid UTF-8 sequences; original bytes that are
    // not `%XX` are passed through unchanged, preserving UTF-8 validity.
    // Decoded bytes may not be valid UTF-8, so fall back to lossy.
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _           => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Method;
    use crate::http::request::types::Version;

    // ---- parse_request_line ------------------------------------------------

    #[test]
    fn parses_simple_get() {
        let (m, p, q, v) = parse_request_line("GET / HTTP/1.1").unwrap();
        assert_eq!(m, Method::Get);
        assert_eq!(p, "/");
        assert_eq!(q, "");
        assert_eq!(v, Version::Http11);
    }

    #[test]
    fn parses_query_string() {
        let (_, p, q, _) = parse_request_line("GET /search?q=hello&page=2 HTTP/1.1").unwrap();
        assert_eq!(p, "/search");
        assert_eq!(q, "q=hello&page=2");
    }

    #[test]
    fn parses_http10() {
        let (_, _, _, v) = parse_request_line("GET / HTTP/1.0").unwrap();
        assert_eq!(v, Version::Http10);
    }

    #[test]
    fn rejects_unknown_method() {
        assert!(matches!(
            parse_request_line("PATCH / HTTP/1.1"),
            Err(ParseError::UnknownMethod(_))
        ));
    }

    #[test]
    fn rejects_unsupported_version() {
        assert!(matches!(
            parse_request_line("GET / HTTP/2.0"),
            Err(ParseError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn rejects_relative_uri() {
        assert!(matches!(
            parse_request_line("GET relative HTTP/1.1"),
            Err(ParseError::BadUri(_))
        ));
    }

    #[test]
    fn rejects_empty_uri() {
        // splitn gives an empty string for the URI field.
        assert!(parse_request_line("GET  HTTP/1.1").is_err());
    }

    #[test]
    fn percent_decode_basic() {
        let (_, path, _, _) = parse_request_line("GET /hello%20world HTTP/1.1").unwrap();
        assert_eq!(path, "/hello world");
    }

    #[test]
    fn percent_decode_invalid_sequence_passthrough() {
        // %ZZ is not valid hex — should be left as-is.
        let decoded = super::percent_decode("/path%ZZend");
        assert_eq!(decoded, "/path%ZZend");
    }

    // ---- parse_headers -----------------------------------------------------

    #[test]
    fn parses_basic_headers() {
        let lines = vec!["Host: example.com", "Content-Type: text/plain"];
        let map = parse_headers(&lines).unwrap();
        assert_eq!(map.get("host"), Some("example.com"));
        assert_eq!(map.get("content-type"), Some("text/plain"));
    }

    #[test]
    fn trims_header_value_whitespace() {
        let lines = vec!["  X-Custom:   value with spaces   "];
        // Leading spaces trigger fold rejection, so use a valid line.
        let lines = vec!["X-Custom:   value with spaces   "];
        let map = parse_headers(&lines).unwrap();
        assert_eq!(map.get("x-custom"), Some("value with spaces"));
    }

    #[test]
    fn rejects_header_without_colon() {
        let lines = vec!["BadHeader"];
        assert!(parse_headers(&lines).is_err());
    }

    #[test]
    fn rejects_whitespace_before_colon() {
        let lines = vec!["Name : value"];
        assert!(parse_headers(&lines).is_err());
    }

    #[test]
    fn rejects_obsolete_folding() {
        let lines = vec![" continuation value"];
        assert!(parse_headers(&lines).is_err());
    }

    #[test]
    fn skips_empty_lines() {
        let lines = vec!["Host: example.com", "", "Connection: close"];
        let map = parse_headers(&lines).unwrap();
        assert_eq!(map.len(), 2);
    }

    // ---- parse_request_head (integration) ----------------------------------

    #[test]
    fn full_request_head_complete() {
        let raw = b"GET /index.html HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
        match parse_request_head(raw) {
            ParseResult::Complete(req, consumed) => {
                assert_eq!(req.method,  Method::Get);
                assert_eq!(req.path,    "/index.html");
                assert_eq!(consumed,    raw.len());
                assert_eq!(req.headers.get("host"), Some("localhost"));
                assert!(req.is_keep_alive());
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn partial_request_head_returns_incomplete() {
        let raw = b"GET / HTTP/1.1\r\nHost: localhost";
        assert!(matches!(parse_request_head(raw), ParseResult::Incomplete));
    }

    #[test]
    fn oversized_header_block_returns_error() {
        let big = vec![b'A'; MAX_HEADER_BYTES + 1];
        assert!(matches!(
            parse_request_head(&big),
            ParseResult::Error(ParseError::HeadersTooLarge)
        ));
    }

    #[test]
    fn consumed_offset_with_body_prefix() {
        // Simulate a POST where body bytes immediately follow the headers.
        let raw = b"POST /upload HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        match parse_request_head(raw) {
            ParseResult::Complete(_, consumed) => {
                // Consumed should end right after `\r\n\r\n`, leaving "hello".
                assert_eq!(&raw[consumed..], b"hello");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn post_request_with_content_length() {
        let raw = b"POST /data HTTP/1.1\r\nContent-Length: 3\r\nHost: x\r\n\r\n";
        match parse_request_head(raw) {
            ParseResult::Complete(req, _) => {
                assert_eq!(req.method, Method::Post);
                assert_eq!(req.content_length(), Some(3));
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn delete_request_parsed() {
        let raw = b"DELETE /resource/1 HTTP/1.1\r\nHost: api.example.com\r\n\r\n";
        match parse_request_head(raw) {
            ParseResult::Complete(req, _) => {
                assert_eq!(req.method, Method::Delete);
                assert_eq!(req.path, "/resource/1");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    // ---- find_header_end ---------------------------------------------------

    #[test]
    fn finds_double_crlf() {
        let buf = b"HEAD\r\n\r\n";
        assert_eq!(super::find_header_end(buf), Some(4));
    }

    #[test]
    fn returns_none_without_double_crlf() {
        let buf = b"HEAD\r\n";
        assert_eq!(super::find_header_end(buf), None);
    }
}