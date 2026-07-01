/// DELETE resource handler.
///
/// Removes a file (or empty directory) at the path resolved from the request
/// URL against the route root. Returns:
///
/// | Condition                          | Response        |
/// |------------------------------------|-----------------|
/// | File deleted successfully          | 204 No Content  |
/// | Target is a non-empty directory    | 409 Conflict    |
/// | Target does not exist              | 404 Not Found   |
/// | Path traversal detected            | 404 Not Found   |
/// | Resolved path is a directory root  | 403 Forbidden   |
/// | Permission denied                  | 403 Forbidden   |
/// | Route has no root configured       | 403 Forbidden   |
///
/// # Safety
///
/// - All paths go through `resolve_safe` — traversal to files outside
///   `route.root` is blocked.
/// - Deleting the root directory itself is explicitly forbidden (a request
///   for `/` resolves to the root; we compare and refuse).
/// - Only regular files and **empty** directories may be deleted. Recursive
///   deletion is never performed.
use std::path::Path;

use crate::config::types::RouteConfig;
use crate::http::request::types::Request;
use crate::http::response::{builder, types::Response};
use crate::utils::path as pathutil;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Handle a DELETE request.
pub fn handle(request: &Request, route: &RouteConfig) -> Response {
    let root = match &route.root {
        Some(r) => r.as_str(),
        None    => return builder::forbidden(None),
    };

    // Safe path resolution (blocks traversal).
    let fs_path = match pathutil::resolve_safe(root, &request.path) {
        Ok(p)  => p,
        Err(_) => return builder::not_found(None),
    };

    // Refuse to delete the root directory itself.
    let canonical_root = match Path::new(root).canonicalize() {
        Ok(p)  => p,
        Err(_) => return builder::not_found(None),
    };
    if fs_path == canonical_root {
        return builder::forbidden(None);
    }

    // Dispatch based on target type.
    if fs_path.is_file() {
        delete_file(&fs_path)
    } else if fs_path.is_dir() {
        delete_directory(&fs_path)
    } else {
        builder::not_found(None)
    }
}

// ---------------------------------------------------------------------------
// Deletion helpers
// ---------------------------------------------------------------------------

fn delete_file(path: &Path) -> Response {
    match std::fs::remove_file(path) {
        Ok(())  => builder::no_content(),
        Err(e)  => map_io_error_to_response(e),
    }
}

fn delete_directory(path: &Path) -> Response {
    // Only allow deleting empty directories.
    match std::fs::remove_dir(path) {
        Ok(()) => builder::no_content(),
        Err(e) => {
            if e.raw_os_error() == Some(libc::ENOTEMPTY) {
                conflict_response()
            } else {
                map_io_error_to_response(e)
            }
        }
    }
}

fn map_io_error_to_response(e: std::io::Error) -> Response {
    match e.kind() {
        std::io::ErrorKind::NotFound        => builder::not_found(None),
        std::io::ErrorKind::PermissionDenied => builder::forbidden(None),
        _                                   => builder::internal_server_error(None),
    }
}

/// 409 Conflict — directory is not empty.
fn conflict_response() -> Response {
    let body = b"<html><head><title>409 Conflict</title></head>\
                 <body><h1>409 Conflict</h1>\
                 <p>Cannot delete a non-empty directory.</p>\
                 </body></html>".to_vec();
    let resp = builder::error(crate::http::response::types::StatusCode::CONFLICT, body);
    resp
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
        let dir = format!("/tmp/delete_test_{}", unsafe { libc::getpid() });
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn delete_req(path: &str) -> Request {
        Request {
            method:  Method::Delete,
            path:    path.into(),
            query:   String::new(),
            version: Version::Http11,
            headers: HeaderMap::new(),
            body:    Vec::new(),
        }
    }

    fn route_for(root: &str) -> RouteConfig {
        RouteConfig {
            path: "/".into(),
            root: Some(root.into()),
            ..Default::default()
        }
    }

    // ---- file deletion -----------------------------------------------------

    #[test]
    fn delete_existing_file_returns_204() {
        let dir  = tmp_dir();
        let file = format!("{dir}/target.txt");
        fs::write(&file, b"data").unwrap();

        let resp = handle(&delete_req("/target.txt"), &route_for(&dir));
        assert_eq!(resp.status.code(), 204);
        assert!(!fs::metadata(&file).is_ok(), "file should be removed");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_missing_file_returns_404() {
        let dir  = tmp_dir();
        let resp = handle(&delete_req("/ghost.txt"), &route_for(&dir));
        assert_eq!(resp.status.code(), 404);
        fs::remove_dir_all(&dir).ok();
    }

    // ---- directory deletion ------------------------------------------------

    #[test]
    fn delete_empty_directory_returns_204() {
        let dir    = tmp_dir();
        let subdir = format!("{dir}/empty_dir");
        fs::create_dir(&subdir).unwrap();

        let resp = handle(&delete_req("/empty_dir"), &route_for(&dir));
        assert_eq!(resp.status.code(), 204);
        assert!(!fs::metadata(&subdir).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_non_empty_directory_returns_409() {
        let dir    = tmp_dir();
        let subdir = format!("{dir}/has_files");
        fs::create_dir(&subdir).unwrap();
        fs::write(format!("{subdir}/inner.txt"), b"x").unwrap();

        let resp = handle(&delete_req("/has_files"), &route_for(&dir));
        assert_eq!(resp.status.code(), 409);
        fs::remove_dir_all(&dir).ok();
    }

    // ---- safety checks -----------------------------------------------------

    #[test]
    fn cannot_delete_root_directory() {
        let dir  = tmp_dir();
        let resp = handle(&delete_req("/"), &route_for(&dir));
        assert_eq!(resp.status.code(), 403);
        // Root still exists.
        assert!(fs::metadata(&dir).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn traversal_attempt_returns_404() {
        let dir  = tmp_dir();
        let resp = handle(&delete_req("/../etc/passwd"), &route_for(&dir));
        assert_eq!(resp.status.code(), 404);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_without_route_root_returns_403() {
        let route = RouteConfig { path: "/".into(), root: None, ..Default::default() };
        let resp  = handle(&delete_req("/anything"), &route);
        assert_eq!(resp.status.code(), 403);
    }

    // ---- idempotency -------------------------------------------------------

    #[test]
    fn delete_same_file_twice_second_is_404() {
        let dir  = tmp_dir();
        let file = format!("{dir}/once.txt");
        fs::write(&file, b"x").unwrap();

        let r1 = handle(&delete_req("/once.txt"), &route_for(&dir));
        let r2 = handle(&delete_req("/once.txt"), &route_for(&dir));

        assert_eq!(r1.status.code(), 204);
        assert_eq!(r2.status.code(), 404);
        fs::remove_dir_all(&dir).ok();
    }
}