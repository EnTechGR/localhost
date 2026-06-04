/// Directory listing handler.
///
/// Renders an HTML index page for a directory when `directory_listing` is
/// enabled on the route. Extracted from `static_file` into its own module
/// so it can be called directly and tested independently.
use std::path::Path;

use crate::config::types::RouteConfig;
use crate::http::request::types::Request;
use crate::http::response::{builder, types::Response};
use crate::utils::path as pathutil;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Serve an HTML directory listing for the path resolved from `request`.
///
/// Returns:
/// - `200 OK` with an HTML body listing directory entries.
/// - `403 Forbidden` if listing is disabled or a permission error occurs.
/// - `404 Not Found` if the directory does not exist.
pub fn serve(request: &Request, route: &RouteConfig) -> Response {
    if !route.directory_listing {
        return builder::forbidden(None);
    }

    let root = match &route.root {
        Some(r) => r.as_str(),
        None    => return builder::not_found(None),
    };

    let fs_path = match pathutil::resolve_safe(root, &request.path) {
        Ok(p)  => p,
        Err(_) => return builder::not_found(None),
    };

    if !fs_path.is_dir() {
        return builder::not_found(None);
    }

    render_listing(&fs_path, &request.path)
}

// ---------------------------------------------------------------------------
// HTML rendering
// ---------------------------------------------------------------------------

/// Generate the HTML listing body for `dir`.
/// `url_path` is used in the page title and breadcrumb.
pub fn render_listing(dir: &Path, url_path: &str) -> Response {
    let entries = match std::fs::read_dir(dir) {
        Ok(e)  => e,
        Err(_) => return builder::forbidden(None),
    };

    let mut items: Vec<DirEntry> = entries
        .filter_map(|e| e.ok())
        .map(|e| {
            let name   = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let size   = if is_dir {
                None
            } else {
                e.metadata().ok().map(|m| m.len())
            };
            DirEntry { name, is_dir, size }
        })
        .collect();

    // Sort: directories first, then files, both alphabetically.
    items.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name))
    });

    let rows = build_rows(&items);

    let title   = html_escape(url_path);
    let up_link = if url_path != "/" {
        r#"<tr><td colspan="3"><a href="../">../</a></td></tr>"#.to_string()
    } else {
        String::new()
    };

    let body = format!(
        "<!DOCTYPE html>\n\
         <html>\n\
         <head>\n\
           <meta charset=\"utf-8\">\n\
           <title>Index of {title}</title>\n\
           <style>\n\
             body {{ font-family: monospace; margin: 2em; }}\n\
             h1   {{ font-size: 1.2em; }}\n\
             table {{ border-collapse: collapse; }}\n\
             th, td {{ padding: 0.3em 1.5em 0.3em 0; text-align: left; }}\n\
             tr:hover {{ background: #f5f5f5; }}\n\
             .size {{ text-align: right; font-size: 0.9em; color: #555; }}\n\
           </style>\n\
         </head>\n\
         <body>\n\
           <h1>Index of {title}</h1>\n\
           <table>\n\
             <tr><th>Name</th><th>Type</th><th class=\"size\">Size</th></tr>\n\
             {up_link}\n\
             {rows}\
           </table>\n\
         </body>\n\
         </html>\n"
    ).into_bytes();

    builder::ok(body, "text/html; charset=utf-8")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct DirEntry {
    name:   String,
    is_dir: bool,
    size:   Option<u64>,
}

fn build_rows(items: &[DirEntry]) -> String {
    let mut out = String::new();
    for entry in items {
        let href = if entry.is_dir {
            format!("{}/", html_escape(&entry.name))
        } else {
            html_escape(&entry.name)
        };
        let kind = if entry.is_dir { "directory" } else { "file" };
        let size_str = match entry.size {
            Some(n) => format_size(n),
            None    => "-".to_string(),
        };
        out.push_str(&format!(
            "    <tr>\
               <td><a href=\"{href}\">{href}</a></td>\
               <td>{kind}</td>\
               <td class=\"size\">{size_str}</td>\
             </tr>\n"
        ));
    }
    out
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Escape `<`, `>`, `&`, `"` for safe HTML embedding.
fn html_escape(s: &str) -> String {
    s.chars().flat_map(|c| match c {
        '<'  => "&lt;".chars().collect::<Vec<_>>(),
        '>'  => "&gt;".chars().collect(),
        '&'  => "&amp;".chars().collect(),
        '"'  => "&quot;".chars().collect(),
        other => vec![other],
    }).collect()
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
        let dir = format!("/tmp/dir_test_{}", unsafe { libc::getpid() });
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn get_req(path: &str) -> Request {
        Request {
            method:  Method::Get,
            path:    path.into(),
            query:   String::new(),
            version: Version::Http11,
            headers: HeaderMap::new(),
            body:    Vec::new(),
        }
    }

    fn listing_route(root: &str) -> RouteConfig {
        RouteConfig {
            path:              "/".into(),
            root:              Some(root.into()),
            directory_listing: true,
            ..Default::default()
        }
    }

    // ---- serve -------------------------------------------------------------

    #[test]
    fn listing_disabled_returns_403() {
        let dir   = tmp_dir();
        let route = RouteConfig {
            path: "/".into(), root: Some(dir.clone()), directory_listing: false,
            ..Default::default()
        };
        let resp = serve(&get_req("/"), &route);
        assert_eq!(resp.status.code(), 403);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn listing_returns_200_with_html() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/a.txt"), b"").unwrap();
        let resp = serve(&get_req("/"), &listing_route(&dir));
        assert_eq!(resp.status.code(), 200);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("a.txt"));
        assert!(body.contains("<!DOCTYPE html>"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directories_listed_before_files() {
        let dir = tmp_dir();
        fs::create_dir(format!("{dir}/zzz_dir")).unwrap();
        fs::write(format!("{dir}/aaa.txt"), b"").unwrap();
        let resp = serve(&get_req("/"), &listing_route(&dir));
        let body = String::from_utf8_lossy(&resp.body);
        let dir_pos  = body.find("zzz_dir").unwrap();
        let file_pos = body.find("aaa.txt").unwrap();
        assert!(dir_pos < file_pos, "dirs should come before files");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parent_link_present_for_non_root() {
        let dir = tmp_dir();
        // The subdir must exist on disk for resolve_safe to succeed.
        fs::create_dir(format!("{dir}/subdir")).unwrap();
        let resp = serve(&get_req("/subdir"), &listing_route(&dir));
        let body = String::from_utf8_lossy(&resp.body);
        assert!(body.contains("../"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_parent_link_at_root() {
        let dir  = tmp_dir();
        let resp = serve(&get_req("/"), &listing_route(&dir));
        let body = String::from_utf8_lossy(&resp.body);
        assert!(!body.contains("href=\"../\""), "root should have no ../ link");
        fs::remove_dir_all(&dir).ok();
    }

    // ---- html_escape -------------------------------------------------------

    #[test]
    fn html_escape_encodes_special_chars() {
        assert_eq!(html_escape("<b>&\""), "&lt;b&gt;&amp;&quot;");
        assert_eq!(html_escape("normal"), "normal");
    }

    // ---- format_size -------------------------------------------------------

    #[test]
    fn format_size_boundaries() {
        assert_eq!(format_size(0),             "0 B");
        assert_eq!(format_size(1023),          "1023 B");
        assert_eq!(format_size(1024),          "1.0 KiB");
        assert_eq!(format_size(1024 * 1024),   "1.0 MiB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GiB");
    }

    // ---- traversal ---------------------------------------------------------

    #[test]
    fn traversal_in_listing_returns_404() {
        let dir  = tmp_dir();
        let resp = serve(&get_req("/../etc"), &listing_route(&dir));
        assert_eq!(resp.status.code(), 404);
        fs::remove_dir_all(&dir).ok();
    }
}