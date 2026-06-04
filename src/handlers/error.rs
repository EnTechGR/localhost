/// Error page rendering.
///
/// Loads and returns custom error page content from disk (configured via
/// `error_page` directives), or falls back to a built-in HTML default.
///
/// All functions are pure from the caller's perspective: they return a
/// `Response`; the caller does not need to distinguish "loaded from disk" from
/// "built-in default".
use crate::config::types::ServerConfig;
use crate::http::response::{builder, types::{Response, StatusCode}};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Render an error response for `code`.
///
/// Attempts to load the custom error page configured on `server` for `code`.
/// Falls back to a built-in HTML page if the file is absent or unreadable.
pub fn render(code: u16, server: &ServerConfig) -> Response {
    let custom_content = server
        .error_page(code)
        .and_then(|path| std::fs::read_to_string(path).ok());

    match code {
        400 => builder::bad_request(custom_content.as_deref().unwrap_or("Bad Request")),
        403 => builder::forbidden(custom_content.as_deref()),
        404 => builder::not_found(custom_content.as_deref()),
        405 => {
            // method_not_allowed needs an Allow header — caller should use
            // builder::method_not_allowed directly; this is a fallback.
            render_generic(StatusCode::METHOD_NOT_ALLOWED, custom_content.as_deref())
        }
        413 => builder::payload_too_large(),
        500 => builder::internal_server_error(custom_content.as_deref()),
        _   => render_generic(StatusCode(code), custom_content.as_deref()),
    }
}

/// Render a generic error response for `code` with optional custom page body.
///
/// Used for uncommon error codes that don't have a dedicated builder function.
pub fn render_generic(status: StatusCode, custom_content: Option<&str>) -> Response {
    let body = match custom_content {
        Some(c) => c.as_bytes().to_vec(),
        None    => default_html(status).into_bytes(),
    };
    let mut resp = Response::new(status);
    resp.headers.set("Content-Type",   "text/html; charset=utf-8");
    resp.headers.set("Content-Length", body.len().to_string());
    resp.body = body;
    resp
}

// ---------------------------------------------------------------------------
// Default HTML pages
// ---------------------------------------------------------------------------

/// Default HTML body for a given status code.
pub fn default_html(status: StatusCode) -> String {
    format!(
        "<!DOCTYPE html>\n\
         <html>\n\
         <head><meta charset=\"utf-8\"><title>{code} {reason}</title></head>\n\
         <body>\n\
           <h1>{code} {reason}</h1>\n\
         </body>\n\
         </html>\n",
        code   = status.code(),
        reason = status.reason(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ServerConfig;
    use std::fs;

    fn server_with_error_page(code: u16, path: &str) -> ServerConfig {
        let mut s = ServerConfig::default();
        s.error_pages.insert(code, path.to_string());
        s
    }

    fn tmp_error_page(code: u16, content: &str) -> (String, String) {
        let dir  = format!("/tmp/errpage_test_{}", unsafe { libc::getpid() });
        fs::create_dir_all(&dir).unwrap();
        let path = format!("{dir}/{code}.html");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    // ---- render with custom page -------------------------------------------

    #[test]
    fn render_404_custom_page_loaded() {
        let (dir, path) = tmp_error_page(404, "<h1>Custom 404</h1>");
        let server      = server_with_error_page(404, &path);
        let resp        = render(404, &server);
        assert_eq!(resp.status.code(), 404);
        assert_eq!(resp.body, b"<h1>Custom 404</h1>");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_500_custom_page_loaded() {
        let (dir, path) = tmp_error_page(500, "<h1>Server Error</h1>");
        let server      = server_with_error_page(500, &path);
        let resp        = render(500, &server);
        assert_eq!(resp.status.code(), 500);
        assert_eq!(resp.body, b"<h1>Server Error</h1>");
        fs::remove_dir_all(&dir).ok();
    }

    // ---- render without custom page ----------------------------------------

    #[test]
    fn render_404_default_when_no_page_configured() {
        let server = ServerConfig::default();
        let resp   = render(404, &server);
        assert_eq!(resp.status.code(), 404);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("404"), "body should mention 404: {body}");
        // builder.rs default pages use <html>; error.rs default_html uses <!DOCTYPE html>.
        // Either is acceptable — just verify we got a non-empty HTML response.
        assert!(body.contains("<html") || body.contains("<!DOCTYPE"), "body should be HTML: {body}");
    }

    #[test]
    fn render_403_default() {
        let server = ServerConfig::default();
        let resp   = render(403, &server);
        assert_eq!(resp.status.code(), 403);
    }

    #[test]
    fn render_400_default() {
        let server = ServerConfig::default();
        let resp   = render(400, &server);
        assert_eq!(resp.status.code(), 400);
    }

    #[test]
    fn render_413_default() {
        let server = ServerConfig::default();
        let resp   = render(413, &server);
        assert_eq!(resp.status.code(), 413);
    }

    // ---- missing custom page file falls back to default -------------------

    #[test]
    fn render_falls_back_when_custom_page_file_missing() {
        let server = server_with_error_page(404, "/nonexistent/path/404.html");
        let resp   = render(404, &server);
        assert_eq!(resp.status.code(), 404);
        // Body should be the built-in default, not empty.
        assert!(!resp.body.is_empty());
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("<!DOCTYPE html>") || body.contains("404"));
    }

    // ---- render_generic ----------------------------------------------------

    #[test]
    fn render_generic_unknown_code() {
        let resp = render_generic(StatusCode(418), None);
        assert_eq!(resp.status.code(), 418);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("418"));
    }

    #[test]
    fn render_generic_with_custom_content() {
        let resp = render_generic(StatusCode(503), Some("<h1>Down for maintenance</h1>"));
        assert_eq!(resp.body, b"<h1>Down for maintenance</h1>");
    }

    // ---- default_html format -----------------------------------------------

    #[test]
    fn default_html_includes_code_and_reason() {
        let html = default_html(StatusCode::NOT_FOUND);
        assert!(html.contains("404"));
        assert!(html.contains("Not Found"));
        assert!(html.contains("<!DOCTYPE html>"));
    }

    // ---- content-length consistency ----------------------------------------

    #[test]
    fn content_length_matches_body_for_all_standard_codes() {
        let server = ServerConfig::default();
        for &code in &[400u16, 403, 404, 413, 500] {
            let resp = render(code, &server);
            let cl: usize = resp.headers
                .get("Content-Length")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(cl, resp.body.len(), "Content-Length mismatch for {code}");
        }
    }
}