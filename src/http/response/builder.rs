/// HTTP response factory functions.
///
/// Each function constructs a complete `Response` with appropriate headers
/// for a specific scenario. The caller then passes the `Response` to
/// `writer::serialize` for wire serialisation.
///
/// All builder functions are pure — no I/O, no allocation beyond the response
/// itself, no panics on any input.
use crate::config::types::Method;
use crate::http::response::types::{Response, ResponseHeaders, StatusCode};

// ---------------------------------------------------------------------------
// Standard responses
// ---------------------------------------------------------------------------

/// 200 OK with an explicit body and MIME type.
pub fn ok(body: Vec<u8>, mime: &str) -> Response {
    let mut resp = Response::new(StatusCode::OK);
    resp.headers.set("Content-Type",   mime);
    resp.headers.set("Content-Length", body.len().to_string());
    resp.body = body;
    resp
}

/// 201 Created — used after a successful file upload.
pub fn created(location: Option<&str>) -> Response {
    let mut resp = Response::new(StatusCode::CREATED);
    if let Some(loc) = location {
        resp.headers.set("Location",       loc);
        resp.headers.set("Content-Length", "0");
    }
    resp
}

/// 204 No Content — used after a successful DELETE with no body.
pub fn no_content() -> Response {
    let mut resp = Response::new(StatusCode::NO_CONTENT);
    resp.headers.set("Content-Length", "0");
    resp
}

/// 3xx redirect.
///
/// `code` must be one of 301, 302, 307, 308 — caller is responsible for
/// using a valid redirect code. Sends a minimal HTML body pointing at the
/// new location.
pub fn redirect(code: u16, location: &str) -> Response {
    let status = StatusCode(code);
    let body   = format!(
        "<html><body>Redirecting to <a href=\"{location}\">{location}</a></body></html>"
    ).into_bytes();

    let mut resp = Response::new(status);
    resp.headers.set("Location",       location);
    resp.headers.set("Content-Type",   "text/html; charset=utf-8");
    resp.headers.set("Content-Length", body.len().to_string());
    resp.body = body;
    resp
}

/// 400 Bad Request.
pub fn bad_request(detail: &str) -> Response {
    error_response(StatusCode::BAD_REQUEST, detail)
}

/// 403 Forbidden.
pub fn forbidden(custom_page: Option<&str>) -> Response {
    error_from_page(StatusCode::FORBIDDEN, custom_page)
}

/// 404 Not Found, optionally using a custom error page body.
///
/// `error_page` is the **content** of the custom error page (already read
/// from disk by the caller), not a file path.
pub fn not_found(error_page: Option<&str>) -> Response {
    error_from_page(StatusCode::NOT_FOUND, error_page)
}

/// 405 Method Not Allowed.
///
/// The `Allow` header is set to the comma-separated list of `allowed` methods,
/// as required by RFC 9110 §15.5.6.
pub fn method_not_allowed(allowed: &[Method]) -> Response {
    let allow_str: String = allowed
        .iter()
        .map(Method::as_str)
        .collect::<Vec<_>>()
        .join(", ");

    let body = format!(
        "<html><body><h1>405 Method Not Allowed</h1>\
         <p>Allowed: {allow_str}</p></body></html>"
    ).into_bytes();

    let mut resp = Response::new(StatusCode::METHOD_NOT_ALLOWED);
    resp.headers.set("Allow",          allow_str);
    resp.headers.set("Content-Type",   "text/html; charset=utf-8");
    resp.headers.set("Content-Length", body.len().to_string());
    resp.body = body;
    resp
}

/// 413 Payload Too Large.
pub fn payload_too_large() -> Response {
    error_response(StatusCode::PAYLOAD_TOO_LARGE, "Request body exceeds the configured limit.")
}

/// 500 Internal Server Error, optionally using a custom error page body.
pub fn internal_server_error(custom_page: Option<&str>) -> Response {
    error_from_page(StatusCode::INTERNAL_SERVER_ERROR, custom_page)
}

/// Generic numeric error response with a plain-text body.
///
/// Used for unusual codes where we don't have a dedicated builder and
/// there is no custom page configured.
pub fn error(code: u16, body: Vec<u8>) -> Response {
    let mut resp = Response::new(StatusCode(code));
    resp.headers.set("Content-Type",   "text/html; charset=utf-8");
    resp.headers.set("Content-Length", body.len().to_string());
    resp.body = body;
    resp
}

// ---------------------------------------------------------------------------
// CGI response
// ---------------------------------------------------------------------------

/// Parse a raw CGI output byte stream into a `Response`.
///
/// CGI scripts output their own headers (e.g. `Content-Type: text/html`)
/// followed by `\r\n\r\n` (or `\n\n`), then the body. The `Status:` header
/// (non-standard, CGI/1.1 §6.3.3) sets the HTTP status code.
///
/// Returns `Err` if the CGI output has no header section.
pub fn from_cgi_output(raw: Vec<u8>) -> Result<Response, &'static str> {
    // Accept both \r\n\r\n and \n\n as the header/body separator.
    let (header_end, body_start) = if let Some(pos) = find_seq(&raw, b"\r\n\r\n") {
        (pos, pos + 4)
    } else if let Some(pos) = find_seq(&raw, b"\n\n") {
        (pos, pos + 2)
    } else {
        return Err("CGI output missing header/body separator");
    };

    let header_section = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| "CGI headers are not valid UTF-8")?;

    let mut status = StatusCode::OK;
    let mut headers = ResponseHeaders::new();

    for line in header_section.lines() {
        if line.is_empty() { continue; }

        let colon = line.find(':').ok_or("CGI header missing ':'")?;
        let name  = line[..colon].trim();
        let value = line[colon + 1..].trim();

        if name.eq_ignore_ascii_case("Status") {
            // "Status: 404 Not Found" — extract the numeric code.
            if let Some(code_str) = value.split_whitespace().next() {
                if let Ok(code) = code_str.parse::<u16>() {
                    status = StatusCode(code);
                    continue; // do not forward Status: header to client
                }
            }
        }
        headers.append(name, value);
    }

    let body = raw[body_start..].to_vec();

    // If the CGI didn't set Content-Length, add it now.
    if headers.get("Content-Length").is_none() {
        headers.set("Content-Length", body.len().to_string());
    }

    Ok(Response { status, headers, body })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Build an error response from an optional pre-read page body.
fn error_from_page(status: StatusCode, page_content: Option<&str>) -> Response {
    let body = match page_content {
        Some(content) => content.as_bytes().to_vec(),
        None          => default_error_html(status).into_bytes(),
    };
    let mime = if page_content.is_some() { "text/html; charset=utf-8" } else { "text/html; charset=utf-8" };

    let mut resp = Response::new(status);
    resp.headers.set("Content-Type",   mime);
    resp.headers.set("Content-Length", body.len().to_string());
    resp.body = body;
    resp
}

/// Build an error response from a string detail message (no custom page).
fn error_response(status: StatusCode, detail: &str) -> Response {
    let body = default_error_html_with_detail(status, detail).into_bytes();
    let mut resp = Response::new(status);
    resp.headers.set("Content-Type",   "text/html; charset=utf-8");
    resp.headers.set("Content-Length", body.len().to_string());
    resp.body = body;
    resp
}

fn default_error_html(status: StatusCode) -> String {
    format!(
        "<html><head><title>{status}</title></head>\
         <body><h1>{status}</h1></body></html>"
    )
}

fn default_error_html_with_detail(status: StatusCode, detail: &str) -> String {
    format!(
        "<html><head><title>{status}</title></head>\
         <body><h1>{status}</h1><p>{detail}</p></body></html>"
    )
}

/// Find `needle` in `haystack`, return the index of the first occurrence.
fn find_seq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Method;

    #[test]
    fn ok_sets_content_length() {
        let resp = ok(b"hello".to_vec(), "text/plain");
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.headers.get("Content-Length"), Some("5"));
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn redirect_sets_location_and_body() {
        let resp = redirect(301, "/new-path");
        assert_eq!(resp.status, StatusCode::MOVED_PERMANENTLY);
        assert_eq!(resp.headers.get("Location"), Some("/new-path"));
        assert!(!resp.body.is_empty());
    }

    #[test]
    fn not_found_default_html() {
        let resp = not_found(None);
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
        assert!(resp.body.windows(5).any(|w| w == b"<html"));
    }

    #[test]
    fn not_found_custom_page() {
        let custom = "<html><body>Custom 404</body></html>";
        let resp = not_found(Some(custom));
        assert_eq!(resp.body, custom.as_bytes());
    }

    #[test]
    fn method_not_allowed_sets_allow_header() {
        let resp = method_not_allowed(&[Method::Get, Method::Head]);
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
        let allow = resp.headers.get("Allow").unwrap();
        assert!(allow.contains("GET"));
        assert!(allow.contains("HEAD"));
    }

    #[test]
    fn error_builder_with_code() {
        let body = b"<h1>503</h1>".to_vec();
        let resp = error(503, body.clone());
        assert_eq!(resp.status.code(), 503);
        assert_eq!(resp.body, body);
    }

    #[test]
    fn from_cgi_output_parses_status_header() {
        let raw = b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nNot here";
        match from_cgi_output(raw.to_vec()) {
            Ok(resp) => {
                assert_eq!(resp.status.code(), 404);
                assert_eq!(resp.body, b"Not here");
                // Status: header must not be forwarded.
                assert!(resp.headers.get("Status").is_none());
            }
            Err(e) => panic!("expected Ok, got error: {e}"),
        }
    }

    #[test]
    fn from_cgi_output_default_200() {
        let raw = b"Content-Type: text/html\r\n\r\n<h1>CGI</h1>";
        let resp = from_cgi_output(raw.to_vec()).unwrap();
        assert_eq!(resp.status.code(), 200);
        assert_eq!(resp.body, b"<h1>CGI</h1>");
    }

    #[test]
    fn from_cgi_output_lf_separator() {
        // Some CGI scripts use \n\n instead of \r\n\r\n.
        let raw = b"Content-Type: text/plain\n\nHello";
        let resp = from_cgi_output(raw.to_vec()).unwrap();
        assert_eq!(resp.body, b"Hello");
    }

    #[test]
    fn from_cgi_output_no_separator_is_error() {
        let raw = b"Content-Type: text/plain";
        assert!(from_cgi_output(raw.to_vec()).is_err());
    }

    #[test]
    fn from_cgi_output_adds_content_length() {
        let raw = b"Content-Type: text/plain\r\n\r\nfive!";
        let resp = from_cgi_output(raw.to_vec()).unwrap();
        assert_eq!(resp.headers.get("Content-Length"), Some("5"));
    }

    #[test]
    fn payload_too_large_has_413() {
        let resp = payload_too_large();
        assert_eq!(resp.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn created_without_location() {
        let resp = created(None);
        assert_eq!(resp.status, StatusCode::CREATED);
    }

    #[test]
    fn no_content_has_204() {
        assert_eq!(no_content().status, StatusCode::NO_CONTENT);
    }
}