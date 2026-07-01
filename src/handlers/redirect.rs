/// HTTP redirect handler.
///
/// Constructs redirect responses with validated `Location` headers.
/// Thin wrapper over `builder::redirect` that adds URL safety checks.
use crate::http::response::{builder, types::Response};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Build a redirect response for the given status code and target URL.
///
/// `target` is validated: only relative paths (`/...`) and absolute HTTP/S
/// URLs (`https://...`) are permitted. Targets that look like protocol-relative
/// URLs (`//...`) or data URIs are rejected in favour of a 400 response to
/// prevent open-redirect abuse by configuration mistakes.
///
/// Returns the redirect response, or a `400 Bad Request` if the target fails
/// validation.
pub fn redirect(code: crate::http::response::types::StatusCode, target: &str) -> Response {
    match validate_redirect_target(target) {
        Ok(())  => builder::redirect(code, target),
        Err(reason) => {
            eprintln!("[WARN] redirect: invalid target '{target}': {reason}");
            builder::bad_request("redirect target is not a valid URL")
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate that `target` is safe to use as a `Location` header value.
fn validate_redirect_target(target: &str) -> Result<(), &'static str> {
    if target.is_empty() {
        return Err("empty redirect target");
    }

    // Absolute HTTP/HTTPS URL.
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(());
    }

    // Relative path (must start with /).
    if target.starts_with('/') {
        // Reject protocol-relative "//host" targets.
        if target.starts_with("//") {
            return Err("protocol-relative URLs not allowed");
        }
        return Ok(());
    }

    Err("target must be an absolute path or http(s):// URL")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::response::types::StatusCode;

    #[test]
    fn valid_relative_path() {
        let resp = redirect(StatusCode::MOVED_PERMANENTLY, "/new-path");
        assert_eq!(resp.status.code(), 301);
        assert_eq!(resp.headers.get("Location"), Some("/new-path"));
    }

    #[test]
    fn valid_absolute_https_url() {
        let resp = redirect(StatusCode::FOUND, "https://example.com/page");
        assert_eq!(resp.status.code(), 302);
        assert_eq!(resp.headers.get("Location"), Some("https://example.com/page"));
    }

    #[test]
    fn valid_temporary_redirect_307() {
        let resp = redirect(StatusCode::TEMPORARY_REDIRECT, "/temp");
        assert_eq!(resp.status.code(), 307);
    }

    #[test]
    fn valid_permanent_redirect_308() {
        let resp = redirect(StatusCode::PERMANENT_REDIRECT, "/permanent");
        assert_eq!(resp.status.code(), 308);
    }

    #[test]
    fn empty_target_returns_400() {
        let resp = redirect(StatusCode::MOVED_PERMANENTLY, "");
        assert_eq!(resp.status.code(), 400);
    }

    #[test]
    fn protocol_relative_url_returns_400() {
        let resp = redirect(StatusCode::MOVED_PERMANENTLY, "//evil.com/steal");
        assert_eq!(resp.status.code(), 400);
    }

    #[test]
    fn relative_path_without_leading_slash_returns_400() {
        let resp = redirect(StatusCode::MOVED_PERMANENTLY, "relative/path");
        assert_eq!(resp.status.code(), 400);
    }

    #[test]
    fn javascript_uri_returns_400() {
        // javascript: URIs are not http(s):// and don't start with /
        let resp = redirect(StatusCode::MOVED_PERMANENTLY, "javascript:alert(1)");
        assert_eq!(resp.status.code(), 400);
    }

    #[test]
    fn redirect_body_contains_location_link() {
        let resp = redirect(StatusCode::MOVED_PERMANENTLY, "/new");
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("/new"));
    }
}