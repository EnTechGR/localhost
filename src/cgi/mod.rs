//! CGI/1.1 execution subsystem.
//!
//! This module runs a CGI script in a forked child process and pumps its
//! stdin/stdout through the same `epoll` event loop the rest of the server
//! uses. It is deliberately split so the leaf logic (process spawning, env
//! construction, non-blocking pipe pumping) has **no dependency on the event
//! loop**; the dispatcher drives it from the outside.
//!
//! - [`env`]      — build the CGI/1.1 environment variable list.
//! - [`executor`] — `fork`/`exec` the interpreter, returning live pipe fds.
//! - [`io`]       — non-blocking pumping of the stdin/stdout pipes.
//!
//! # Lifecycle (driven by the dispatcher)
//!
//! ```text
//!   process_request detects a CGI target
//!        │
//!        ▼
//!   executor::spawn()  ──►  CgiProcess { pid, stdin_fd, stdout_fd, … }
//!        │
//!        ├─ register stdout_fd in epoll (EPOLLIN)
//!        ├─ register stdin_fd  in epoll (EPOLLOUT)  [only if a body exists]
//!        └─ connection phase → AwaitingCgi
//!        │
//!   epoll fires on a pipe fd
//!        ├─ stdin  EPOLLOUT ──► io::pump_stdin   (write body, then close → EOF)
//!        └─ stdout EPOLLIN  ──► io::pump_stdout  (accumulate output)
//!        │                         │
//!        │                         └─ on EOF: build_response() → write_buf
//!        ▼
//!   connection phase → WritingResponse (client fd re-armed EPOLLOUT)
//! ```
//!
//! All teardown (closing pipe fds, killing/reaping the child) is the
//! dispatcher's responsibility and must happen on every exit path: success,
//! malformed output, client disconnect, and timeout.
pub mod env;
pub mod executor;
pub mod io;

use std::os::unix::io::RawFd;
use std::path::PathBuf;

use crate::config::types::RouteConfig;
use crate::http::request::types::Request;
use crate::http::response::{builder, types::{Response, StatusCode}};

// ---------------------------------------------------------------------------
// CgiTarget
// ---------------------------------------------------------------------------

/// A resolved CGI invocation: which interpreter to run, on which script,
/// and the directory the child should `chdir` into for correct relative paths.
#[derive(Debug, Clone)]
pub struct CgiTarget {
    /// Absolute path to the interpreter (e.g. `/usr/bin/python3`).
    pub interpreter: String,
    /// Canonical absolute path to the script file (passed as `argv[1]`).
    pub script_path: PathBuf,
    /// Directory containing the script; the child `chdir`s here before exec.
    pub script_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// CgiProcess
// ---------------------------------------------------------------------------

/// A running CGI child process and the non-blocking state of its pipes.
///
/// Owned by the dispatcher (via the registry) for the duration of the CGI
/// exchange. The two pipe fds are registered in epoll; this struct holds the
/// request body still to be written and the output accumulated so far.
#[derive(Debug)]
pub struct CgiProcess {
    /// PID of the forked child (for `waitpid` / `kill`).
    pub pid: libc::pid_t,

    /// Parent's write end of the child's stdin. `None` once the full body has
    /// been written and the pipe closed (which delivers EOF to the child).
    pub stdin_fd: Option<RawFd>,

    /// Parent's read end of the child's stdout.
    pub stdout_fd: RawFd,

    /// Request body to feed to the child's stdin.
    pub body: Vec<u8>,

    /// How many body bytes have been written so far.
    pub body_cursor: usize,

    /// Raw CGI output accumulated from stdout (headers + body, unparsed).
    pub out_buf: Vec<u8>,
}

impl CgiProcess {
    /// Close the stdin pipe if still open, delivering EOF to the child.
    ///
    /// Idempotent. The caller is responsible for first removing `stdin_fd`
    /// from epoll and the registry's fd→owner map.
    pub fn close_stdin(&mut self) {
        if let Some(fd) = self.stdin_fd.take() {
            unsafe { libc::close(fd) };
        }
    }
}

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

/// Decide whether `req` targets a CGI script under `route`.
///
/// Returns `Some(CgiTarget)` only when **all** of the following hold:
/// 1. The request path has a file extension configured as a CGI handler.
/// 2. The route has a `root`.
/// 3. The resolved path is safe (no traversal) and points at an existing file.
///
/// Returning `None` lets the caller fall through to ordinary static handling
/// (which will, for example, produce a 404 for a missing `.py` file).
pub fn cgi_target(req: &Request, route: &RouteConfig) -> Option<CgiTarget> {
    let ext = file_extension(&req.path)?;
    let interpreter = route.cgi_for_extension(&ext.to_lowercase())?.to_string();

    let root = route.root.as_deref()?;
    let script_path = crate::utils::path::resolve_safe(root, &req.path).ok()?;
    if !script_path.is_file() {
        return None;
    }
    let script_dir = script_path.parent()?.to_path_buf();

    Some(CgiTarget {
        interpreter,
        script_path,
        script_dir,
    })
}

// ---------------------------------------------------------------------------
// Output → Response
// ---------------------------------------------------------------------------

/// Convert the raw bytes collected from a finished CGI child into a `Response`.
///
/// Delegates header/body parsing to [`builder::from_cgi_output`]. If the
/// script produced nothing parseable (no header/body separator, non-UTF-8
/// headers, exec failure), we return `502 Bad Gateway` rather than leaking a
/// broken response to the client.
pub fn build_response(out_buf: Vec<u8>) -> Response {
    match builder::from_cgi_output(out_buf) {
        Ok(resp) => resp,
        Err(_) => {
            let body = b"<html><body><h1>502 Bad Gateway</h1>\
                         <p>The CGI script produced no valid response.</p>\
                         </body></html>"
                .to_vec();
            builder::error(StatusCode::BAD_GATEWAY, body)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the file extension (without the dot) from a URL path.
///
/// Returns `None` when there is no extension (e.g. `/`, `/dir`, `/file`).
fn file_extension(path: &str) -> Option<&str> {
    let filename = path.rsplit('/').next().unwrap_or(path);
    filename
        .rsplit('.')
        .next()
        .filter(|ext| !ext.is_empty() && *ext != filename)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::RouteConfig;
    use crate::http::request::types::{HeaderMap, Method, Request, Version};
    use std::collections::HashMap;
    use std::fs;

    fn req(path: &str) -> Request {
        Request {
            method: Method::Get,
            path: path.into(),
            query: String::new(),
            version: Version::Http11,
            headers: HeaderMap::new(),
            body: Vec::new(),
        }
    }

    fn tmp_dir() -> String {
        let dir = format!("/tmp/cgi_target_{}", unsafe { libc::getpid() });
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cgi_route(root: &str) -> RouteConfig {
        let mut exts = HashMap::new();
        exts.insert("sh".to_string(), "/bin/sh".to_string());
        RouteConfig {
            path: "/".into(),
            root: Some(root.into()),
            cgi_extensions: exts,
            ..Default::default()
        }
    }

    #[test]
    fn resolves_existing_cgi_script() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/run.sh"), b"echo hi").unwrap();
        let route = cgi_route(&dir);

        let target = cgi_target(&req("/run.sh"), &route).expect("should resolve");
        assert_eq!(target.interpreter, "/bin/sh");
        assert!(target.script_path.ends_with("run.sh"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_script_returns_none() {
        let dir = tmp_dir();
        let route = cgi_route(&dir);
        assert!(cgi_target(&req("/nope.sh"), &route).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_cgi_extension_returns_none() {
        let dir = tmp_dir();
        fs::write(format!("{dir}/page.html"), b"<h1>hi</h1>").unwrap();
        let route = cgi_route(&dir);
        assert!(cgi_target(&req("/page.html"), &route).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_response_parses_valid_cgi_output() {
        let raw = b"Content-Type: text/plain\r\n\r\nhello".to_vec();
        let resp = build_response(raw);
        assert_eq!(resp.status.code(), 200);
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn build_response_502_on_garbage() {
        let resp = build_response(b"no separator here".to_vec());
        assert_eq!(resp.status.code(), 502);
    }

    #[test]
    fn file_extension_cases() {
        assert_eq!(file_extension("/cgi-bin/run.py"), Some("py"));
        assert_eq!(file_extension("/run.SH"), Some("SH"));
        assert_eq!(file_extension("/noext"), None);
        assert_eq!(file_extension("/"), None);
    }
}