/// Static file handler.
///
/// Resolves a URL path to a file on disk and returns its contents with the
/// appropriate MIME type. Handles directory index file lookup and optional
/// directory listing.
use std::path::Path;

use crate::config::types::RouteConfig;
use crate::http::request::types::Request;
use crate::http::response::{builder, types::Response};
use crate::utils::{mime, path as pathutil};
use crate::handlers::directory;

// ---------------------------------------------------------------------------
// Public handler
// ---------------------------------------------------------------------------

/// Serve a static file for a GET or HEAD request.
///
/// Resolution order:
/// 1. Resolve `request.path` against `route.root` (safe path check).
/// 2. If the resolved path is a file → serve it.
/// 3. If the resolved path is a directory:
///    a. Try `<dir>/<index_file>` (from route config).
///    b. If `directory_listing` is on, emit an HTML listing.
///    c. Otherwise 403 Forbidden.
/// 4. If the path does not exist → 404 Not Found.
///
/// Returns a fully built `Response`; never panics or returns `Err`.
pub fn serve(request: &Request, route: &RouteConfig, error_page_404: Option<&str>) -> Response {
    let root = match &route.root {
        Some(r) => r.as_str(),
        None    => return builder::not_found(error_page_404),
    };

    // Resolve path safely (blocks ../ traversal).
    let fs_path = match pathutil::resolve_safe(root, &request.path) {
        Ok(p)  => p,
        Err(_) => {
            // Traversal attempt or root does not exist.
            return builder::not_found(error_page_404);
        }
    };

     serve_path(&fs_path, &request.path, route, error_page_404)
}

/// Core serving logic once we have a `PathBuf`.
/// Exported so the directory handler can reuse it for individual entries.
pub fn serve_path(
    fs_path:       &Path,
    url_path:      &str,
    route:         &RouteConfig,
    error_page_404: Option<&str>,
) -> Response {
    if fs_path.is_file() {
        return serve_file(fs_path);
    }

    if fs_path.is_dir() {
        // Try index file.
        let index = fs_path.join(route.effective_index());
        if index.is_file() {
            return serve_file(&index);
        }

        // Directory listing.
        if route.directory_listing {
            return directory::render_listing(fs_path, url_path);
        }

        // No index and listing disabled → 403.
        return builder::forbidden(None);
    }

    // Path does not exist.
    builder::not_found(error_page_404)
}

// ---------------------------------------------------------------------------
// File serving
// ---------------------------------------------------------------------------

fn serve_file(path: &Path) -> Response {
    match std::fs::read(path) {
        Ok(contents) => {
            let mime = mime::from_path(
                path.to_str().unwrap_or(""),
            );
            builder::ok(contents, mime)
        }
        Err(e) => {
            use std::io::ErrorKind::*;
            match e.kind() {
                NotFound    => builder::not_found(None),
                PermissionDenied => builder::forbidden(None),
                _           => builder::internal_server_error(None),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Method, RouteConfig};
    use crate::http::request::types::{HeaderMap, Request, Version};
    use std::fs;

    fn get(path: &str) -> Request {
        Request {
            method:  Method::Get,
            path:    path.into(),
            query:   String::new(),
            version: Version::Http11,
            headers: HeaderMap::new(),
            body:    Vec::new(),
        }
    }

    fn route_for(root: &str) -> RouteConfig {
        RouteConfig {
            path:  "/".into(),
            root:  Some(root.into()),
            ..Default::default()
        }
    }

    fn tmp_dir() -> String {
        let dir = format!("/tmp/static_test_{}", unsafe { libc::getpid() });
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn serves_existing_html_file() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/index.html"), b"<h1>Hello</h1>").unwrap();
        let route = route_for(&dir);
        let resp  = serve(&get("/index.html"), &route, None);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(resp.status.code(), 200);
        assert_eq!(resp.body, b"<h1>Hello</h1>");
        assert_eq!(resp.headers.get("Content-Type"), Some("text/html; charset=utf-8"));
    }

    #[test]
    fn returns_404_for_missing_file() {
        let dir = tmp_dir();
        let route = route_for(&dir);
        let resp  = serve(&get("/missing.html"), &route, None);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 404);
    }

    #[test]
    fn returns_404_on_traversal() {
        let dir = tmp_dir();
        let route = route_for(&dir);
        let resp  = serve(&get("/../etc/passwd"), &route, None);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 404);
    }

    #[test]
    fn serves_index_file_for_directory() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/index.html"), b"index").unwrap();
        let route = RouteConfig {
            path:       "/".into(),
            root:       Some(dir.clone()),
            index_file: Some("index.html".into()),
            ..Default::default()
        };
        let resp = serve(&get("/"), &route, None);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 200);
        assert_eq!(resp.body, b"index");
    }

    #[test]
    fn returns_403_for_directory_without_listing() {
        let dir = tmp_dir();
        let route = RouteConfig {
            path:              "/".into(),
            root:              Some(dir.clone()),
            directory_listing: false,
            ..Default::default()
        };
        let resp = serve(&get("/"), &route, None);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 403);
    }

    #[test]
    fn returns_listing_when_enabled() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/hello.txt"), b"hi").unwrap();
        let route = RouteConfig {
            path:              "/".into(),
            root:              Some(dir.clone()),
            directory_listing: true,
            ..Default::default()
        };
        let resp = serve(&get("/"), &route, None);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.status.code(), 200);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("hello.txt"));
    }

    #[test]
    fn uses_custom_404_page() {
        let dir   = tmp_dir();
        let route = route_for(&dir);
        let resp  = serve(&get("/missing"), &route, Some("<h1>Custom 404</h1>"));
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.body, b"<h1>Custom 404</h1>");
    }

    #[test]
    fn mime_type_from_extension() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/style.css"), b"body{}").unwrap();
        let route = route_for(&dir);
        let resp  = serve(&get("/style.css"), &route, None);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(resp.headers.get("Content-Type"), Some("text/css; charset=utf-8"));
    }
}