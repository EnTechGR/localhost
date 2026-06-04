/// File upload handler (POST).
///
/// Handles two content types:
///
/// 1. `multipart/form-data` — standard HTML `<form enctype="multipart/form-data">`.
///    Each `Content-Disposition: form-data; name="…"; filename="…"` part is
///    written as a separate file in the configured upload directory.
///
/// 2. `application/octet-stream` (or any other type) — raw body upload.
///    The filename is taken from the URL path's last segment, falling back to
///    a sanitised timestamp-based name.
///
/// # Security
///
/// - All destination paths go through `resolve_safe_virtual` — directory
///   traversal via `filename="../../etc/passwd"` is blocked.
/// - Filenames are sanitised: path separators and null bytes are removed.
/// - An explicit `client_body_limit` cap is enforced before writing anything.
/// - Existing files are **overwritten** (use-case: static file deploy). The
///   upstream caller already checked the body size limit.
use std::fs;
use std::path::PathBuf;

use crate::config::types::RouteConfig;
use crate::http::request::types::Request;
use crate::http::response::{builder, types::Response};
use crate::utils::path as pathutil;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Handle a POST upload request.
///
/// Writes one or more files to `route.root` and returns:
/// - `201 Created` with a `Location` header on success.
/// - `400 Bad Request` if the body is malformed.
/// - `403 Forbidden` if the route has no upload root configured.
/// - `413 Payload Too Large` if the body exceeds the limit (should already be
///   caught by the dispatcher, but we re-check defensively).
pub fn handle(request: &Request, route: &RouteConfig, body_limit: usize) -> Response {
    // Guard: must have a root configured.
    let root = match &route.root {
        Some(r) => r.clone(),
        None    => {
            return builder::forbidden(None);
        }
    };

    // Defensive body-size re-check.
    if request.body.len() > body_limit {
        return builder::payload_too_large();
    }

    let content_type = request
        .headers
        .get("content-type")
        .unwrap_or("application/octet-stream");

    if content_type.starts_with("multipart/form-data") {
        handle_multipart(request, &root, content_type)
    } else {
        handle_raw(request, &root)
    }
}

// ---------------------------------------------------------------------------
// Multipart upload
// ---------------------------------------------------------------------------

/// Parse and store all file parts from a `multipart/form-data` body.
fn handle_multipart(request: &Request, root: &str, content_type: &str) -> Response {
    // Extract the boundary token from Content-Type.
    let boundary = match extract_boundary(content_type) {
        Some(b) => b,
        None    => return builder::bad_request("multipart/form-data missing boundary"),
    };

    let parts = match parse_multipart(&request.body, boundary.as_bytes()) {
        Ok(p)  => p,
        Err(e) => return builder::bad_request(e),
    };

    if parts.is_empty() {
        return builder::bad_request("multipart body contained no file parts");
    }

    let mut saved = Vec::new();

    for part in &parts {
        let filename = match &part.filename {
            Some(f) if !f.is_empty() => f.clone(),
            _ => continue, // skip non-file fields silently
        };

        let safe_name = sanitise_filename(&filename);
        let dest = match pathutil::resolve_safe_virtual(root, &format!("/{safe_name}")) {
            Ok(p)  => p,
            Err(_) => return builder::bad_request("invalid upload filename"),
        };

        if let Err(e) = write_file(&dest, &part.data) {
            eprintln!("[ERROR] upload write {dest:?}: {e}");
            return builder::internal_server_error(None);
        }

        saved.push(format!("/{safe_name}"));
    }

    if saved.is_empty() {
        return builder::bad_request("no uploadable file parts found");
    }

    // Return 201 with Location pointing to the first saved file.
    builder::created(Some(&saved[0]))
}

// ---------------------------------------------------------------------------
// Raw body upload
// ---------------------------------------------------------------------------

/// Store the raw request body as a single file.
fn handle_raw(request: &Request, root: &str) -> Response {
    // Derive filename from the URL path's last segment.
    let filename = request.path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("upload");

    let safe_name = sanitise_filename(filename);
    let dest = match pathutil::resolve_safe_virtual(root, &format!("/{safe_name}")) {
        Ok(p)  => p,
        Err(_) => return builder::bad_request("invalid upload path"),
    };

    if let Err(e) = write_file(&dest, &request.body) {
        eprintln!("[ERROR] raw upload write {dest:?}: {e}");
        return builder::internal_server_error(None);
    }

    let location = format!("/{safe_name}");
    builder::created(Some(&location))
}

// ---------------------------------------------------------------------------
// Multipart parser
// ---------------------------------------------------------------------------

/// A single part from a `multipart/form-data` body.
#[derive(Debug)]
struct MultipartPart {
    /// Value of `filename=` in `Content-Disposition`, if present.
    filename: Option<String>,
    /// Raw body bytes of this part.
    data:     Vec<u8>,
}

/// Minimal multipart parser (RFC 2046 §5.1).
///
/// Handles the common browser encoding:
/// ```text
/// --<boundary>\r\n
/// Content-Disposition: form-data; name="file"; filename="photo.jpg"\r\n
/// Content-Type: image/jpeg\r\n
/// \r\n
/// <binary data>\r\n
/// --<boundary>--\r\n
/// ```
fn parse_multipart(body: &[u8], boundary: &[u8]) -> Result<Vec<MultipartPart>, &'static str> {
    // Full boundary delimiter: "--" + boundary_value.
    let delim: Vec<u8> = [b"--", boundary].concat();
    let delim_crlf     = [delim.as_slice(), b"\r\n"].concat();
    let end_delim      = [delim.as_slice(), b"--"].concat();

    let mut parts  = Vec::new();
    let mut cursor = 0usize;

    loop {
        // Find the next delimiter.
        let delim_pos = match memmem(body, &delim_crlf, cursor) {
            Some(p) => p,
            None    => break,
        };

        // Skip past the delimiter line.
        cursor = delim_pos + delim_crlf.len();

        // Check for the closing delimiter.
        if body[delim_pos..].starts_with(&end_delim) {
            break;
        }

        // Find the blank line separating part headers from part body.
        let part_bytes = &body[cursor..];
        let header_end = match memmem(part_bytes, b"\r\n\r\n", 0) {
            Some(p) => p,
            None    => return Err("malformed multipart: missing part header terminator"),
        };

        let header_str = std::str::from_utf8(&part_bytes[..header_end])
            .map_err(|_| "multipart header is not UTF-8")?;

        let filename = extract_filename_from_disposition(header_str);
        let body_start = cursor + header_end + 4; // skip \r\n\r\n

        // The part body ends at the next \r\n--boundary.
        let next_delim_marker = [b"\r\n", delim.as_slice()].concat();
        let body_end = match memmem(body, &next_delim_marker, body_start) {
            Some(p) => p,
            None    => body.len(), // last part, take the rest
        };

        parts.push(MultipartPart {
            filename,
            data: body[body_start..body_end].to_vec(),
        });

        cursor = body_end;
    }

    Ok(parts)
}

/// Extract `boundary=<token>` from a Content-Type value.
fn extract_boundary(content_type: &str) -> Option<String> {
    content_type
        .split(';')
        .skip(1)
        .map(str::trim)
        .find(|p| p.to_lowercase().starts_with("boundary="))
        .map(|p| {
            let val = &p["boundary=".len()..];
            // Strip optional surrounding quotes.
            val.trim_matches('"').to_string()
        })
}

/// Extract `filename="..."` from a `Content-Disposition` header block.
fn extract_filename_from_disposition(headers: &str) -> Option<String> {
    for line in headers.lines() {
        if !line.to_lowercase().starts_with("content-disposition:") {
            continue;
        }
        // Find filename=
        for part in line.split(';') {
            let part = part.trim();
            if part.to_lowercase().starts_with("filename=") {
                let val = &part["filename=".len()..].trim_matches('"');
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write `data` to `dest`, creating parent directories if needed.
fn write_file(dest: &PathBuf, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, data)
}

/// Remove path separators and null bytes from a filename to prevent injection.
fn sanitise_filename(name: &str) -> String {
    name.chars()
        .filter(|&c| c != '/' && c != '\\' && c != '\0')
        .collect()
}

/// Naive but allocation-free `memmem`: find `needle` in `haystack[from..]`.
fn memmem(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::RouteConfig;
    use crate::http::request::types::{HeaderMap, Method, Request, Version};
    use std::fs;

    fn tmp_dir() -> String {
        let dir = format!("/tmp/upload_test_{}", unsafe { libc::getpid() });
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn post_request(path: &str, body: &[u8], content_type: &str) -> Request {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type",   content_type);
        headers.insert("Content-Length", &body.len().to_string());
        Request {
            method:  Method::Post,
            path:    path.into(),
            query:   String::new(),
            version: Version::Http11,
            headers,
            body:    body.to_vec(),
        }
    }

    fn route_for(root: &str) -> RouteConfig {
        RouteConfig {
            path: "/upload".into(),
            root: Some(root.into()),
            ..Default::default()
        }
    }

    // ---- raw upload --------------------------------------------------------

    #[test]
    fn raw_upload_creates_file() {
        let dir  = tmp_dir();
        let req  = post_request("/upload/hello.txt", b"file content", "text/plain");
        let resp = handle(&req, &route_for(&dir), 1_000_000);
        assert_eq!(resp.status.code(), 201);
        let saved = fs::read(format!("{dir}/hello.txt")).unwrap();
        assert_eq!(saved, b"file content");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_upload_overwrites_existing() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/data.bin"), b"old").unwrap();
        let req  = post_request("/upload/data.bin", b"new content", "application/octet-stream");
        let resp = handle(&req, &route_for(&dir), 1_000_000);
        assert_eq!(resp.status.code(), 201);
        assert_eq!(fs::read(format!("{dir}/data.bin")).unwrap(), b"new content");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_upload_fallback_filename() {
        let dir  = tmp_dir();
        // Path with no filename segment → uses "upload".
        let req  = post_request("/", b"data", "application/octet-stream");
        let resp = handle(&req, &route_for(&dir), 1_000_000);
        assert_eq!(resp.status.code(), 201);
        assert!(fs::metadata(format!("{dir}/upload")).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upload_body_exceeding_limit_returns_413() {
        let dir  = tmp_dir();
        let req  = post_request("/upload/big.bin", b"1234567890", "application/octet-stream");
        let resp = handle(&req, &route_for(&dir), 5); // limit 5 bytes
        assert_eq!(resp.status.code(), 413);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upload_without_root_returns_403() {
        let req  = post_request("/upload/file.txt", b"data", "text/plain");
        let route = RouteConfig { path: "/upload".into(), root: None, ..Default::default() };
        let resp = handle(&req, &route, 1_000_000);
        assert_eq!(resp.status.code(), 403);
    }

    // ---- sanitise_filename -------------------------------------------------

    #[test]
    fn sanitise_removes_path_separators() {
        assert_eq!(sanitise_filename("../../etc/passwd"), "....etcpasswd");
        assert_eq!(sanitise_filename("normal.txt"),       "normal.txt");
    }

    #[test]
    fn sanitise_removes_null_bytes() {
        let name = "file\0name.txt";
        assert_eq!(sanitise_filename(name), "filename.txt");
    }

    // ---- boundary extraction -----------------------------------------------

    #[test]
    fn extract_boundary_standard() {
        let ct = "multipart/form-data; boundary=----WebKitFormBoundary123";
        assert_eq!(
            extract_boundary(ct),
            Some("----WebKitFormBoundary123".to_string())
        );
    }

    #[test]
    fn extract_boundary_quoted() {
        let ct = "multipart/form-data; boundary=\"myboundary\"";
        assert_eq!(extract_boundary(ct), Some("myboundary".to_string()));
    }

    #[test]
    fn extract_boundary_missing_returns_none() {
        assert_eq!(extract_boundary("multipart/form-data"), None);
    }

    // ---- multipart parsing -------------------------------------------------

    #[test]
    fn multipart_single_file_part() {
        let boundary = "testboundary";
        let body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             Hello, world!\r\n\
             --{boundary}--\r\n"
        );
        let parts = parse_multipart(body.as_bytes(), boundary.as_bytes()).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].filename.as_deref(), Some("hello.txt"));
        assert_eq!(parts[0].data, b"Hello, world!");
    }

    #[test]
    fn multipart_skips_non_file_fields() {
        let boundary = "bnd";
        let body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"description\"\r\n\
             \r\n\
             some text\r\n\
             --{boundary}\r\n\
             Content-Disposition: form-data; name=\"upload\"; filename=\"img.png\"\r\n\
             Content-Type: image/png\r\n\
             \r\n\
             PNGDATA\r\n\
             --{boundary}--\r\n"
        );
        let parts = parse_multipart(body.as_bytes(), boundary.as_bytes()).unwrap();
        // Only the file part should be saved (field without filename is skipped
        // at the handle_multipart level, not the parser level).
        assert_eq!(parts.len(), 2);
        // description part has no filename.
        assert!(parts[0].filename.is_none());
        assert_eq!(parts[1].filename.as_deref(), Some("img.png"));
    }

    #[test]
    fn multipart_full_upload_integration() {
        let dir      = tmp_dir();
        let boundary = "xboundary";
        let body     = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"f\"; filename=\"up.txt\"\r\n\
             \r\n\
             uploaded\r\n\
             --{boundary}--\r\n"
        );
        let ct  = format!("multipart/form-data; boundary={boundary}");
        let req = post_request("/upload", body.as_bytes(), &ct);
        let resp = handle(&req, &route_for(&dir), 1_000_000);
        assert_eq!(resp.status.code(), 201);
        assert_eq!(fs::read(format!("{dir}/up.txt")).unwrap(), b"uploaded");
        fs::remove_dir_all(&dir).ok();
    }

    // ---- memmem helper -----------------------------------------------------

    #[test]
    fn memmem_finds_needle() {
        assert_eq!(memmem(b"hello world", b"world", 0), Some(6));
        assert_eq!(memmem(b"hello",       b"xyz",   0), None);
        assert_eq!(memmem(b"aabaa",       b"aa",    1), Some(3));
    }
}