//! Spawning the CGI child process.
//!
//! `spawn` sets up two pipes, `fork`s, wires the child's stdin/stdout to the
//! pipe ends, `chdir`s into the script directory, and `execve`s the
//! interpreter with `argv = [interpreter, script]`. The parent keeps the
//! opposite pipe ends, switched to non-blocking, and returns them inside a
//! [`CgiProcess`] for the dispatcher to register in `epoll`.
//!
//! # fork-safety
//!
//! Everything the child touches between `fork` and `execve` is
//! async-signal-safe (`dup2`, `close`, `chdir`, `execve`, `_exit`). All
//! allocation — the `CString`s for `argv`/`envp` and the directory — happens
//! in the parent **before** `fork`, so the child only reads already-built
//! buffers (valid via copy-on-write).
//!
//! # fd hygiene
//!
//! Both pipes are created `O_CLOEXEC`, so a CGI child never inherits another
//! CGI's pipe fds: every CLOEXEC fd is closed automatically by `execve`. The
//! two ends the child needs become fds 0/1 via `dup2`, which clears CLOEXEC,
//! so they survive the exec. (Note: the server's listener / epoll / client
//! sockets must also be created `CLOEXEC` so they don't leak into CGI
//! children — that is handled in the listener/epoll layer.)
use std::ffi::CString;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::time::Instant;

use crate::cgi::{env, CgiProcess, CgiTarget};
use crate::config::types::ServerConfig;
use crate::http::request::types::Request;

/// Why a CGI child could not be started.
#[derive(Debug)]
pub enum CgiError {
    /// `pipe2` failed (errno attached).
    Pipe(i32),
    /// `fork` failed (errno attached).
    Fork(i32),
    /// An argument contained an interior NUL and could not become a `CString`.
    BadArgument,
}

impl std::fmt::Display for CgiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CgiError::Pipe(e) => write!(f, "pipe2 failed: errno {e}"),
            CgiError::Fork(e) => write!(f, "fork failed: errno {e}"),
            CgiError::BadArgument => write!(f, "CGI argument contained a NUL byte"),
        }
    }
}

/// Fork/exec the interpreter for `target` and return the live child handle.
///
/// On success the returned [`CgiProcess`] owns two non-blocking pipe fds
/// (`stdin_fd`, `stdout_fd`) which the caller must register in `epoll` and
/// eventually close. On failure no fds are leaked and no child is left running.
pub fn spawn(
    target: &CgiTarget,
    req: &Request,
    server: &ServerConfig,
    server_port: u16,
    peer: SocketAddr,
) -> Result<CgiProcess, CgiError> {
    // Pipe layout: index 0 = read end, index 1 = write end.
    //   stdin  pipe: child reads sin[0],  parent writes sin[1]
    //   stdout pipe: child writes sout[1], parent reads  sout[0]
    let mut sin = [0 as RawFd; 2];
    let mut sout = [0 as RawFd; 2];

    if unsafe { libc::pipe2(sin.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(CgiError::Pipe(errno()));
    }
    if unsafe { libc::pipe2(sout.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        let e = errno();
        close_all(&[sin[0], sin[1]]);
        return Err(CgiError::Pipe(e));
    }

    // Build exec arguments in the parent, before forking.
    let interp_c = match cstr(&target.interpreter) {
        Ok(c) => c,
        Err(e) => {
            close_all(&[sin[0], sin[1], sout[0], sout[1]]);
            return Err(e);
        }
    };
    let script_c = match cstr(&target.script_path.to_string_lossy()) {
        Ok(c) => c,
        Err(e) => {
            close_all(&[sin[0], sin[1], sout[0], sout[1]]);
            return Err(e);
        }
    };
    let dir_c = match cstr(&target.script_dir.to_string_lossy()) {
        Ok(c) => c,
        Err(e) => {
            close_all(&[sin[0], sin[1], sout[0], sout[1]]);
            return Err(e);
        }
    };

    let argv: [*const libc::c_char; 3] =
        [interp_c.as_ptr(), script_c.as_ptr(), std::ptr::null()];

    let env_pairs = env::build_env(
        req,
        &target.script_path,
        &target.script_path,
        server,
        server_port,
        peer,
    );
    let env_c: Vec<CString> = env_pairs
        .iter()
        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
        .collect();
    let mut envp: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
    envp.push(std::ptr::null());

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = errno();
        close_all(&[sin[0], sin[1], sout[0], sout[1]]);
        return Err(CgiError::Fork(e));
    }

    if pid == 0 {
        // ---------- child ----------
        // Async-signal-safe syscalls only past this point.
        unsafe {
            libc::dup2(sin[0], libc::STDIN_FILENO);
            libc::dup2(sout[1], libc::STDOUT_FILENO);
            // The four original pipe fds are O_CLOEXEC and will be closed by
            // execve; the fds 0/1 created by dup2 are not CLOEXEC and survive.
            libc::chdir(dir_c.as_ptr());
            libc::execve(interp_c.as_ptr(), argv.as_ptr(), envp.as_ptr());
            // Only reached if execve failed.
            libc::_exit(127)
        }
    }

    // ---------- parent ----------
    unsafe {
        libc::close(sin[0]); // child's read end
        libc::close(sout[1]); // child's write end
    }
    set_nonblocking(sin[1]);
    set_nonblocking(sout[0]);

    Ok(CgiProcess {
        pid,
        stdin_fd: Some(sin[1]),
        stdout_fd: sout[0],
        body: req.body.clone(),
        body_cursor: 0,
        out_buf: Vec::new(),
        started: Instant::now(),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cstr(s: &str) -> Result<CString, CgiError> {
    CString::new(s).map_err(|_| CgiError::BadArgument)
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn close_all(fds: &[RawFd]) {
    for &fd in fds {
        unsafe { libc::close(fd) };
    }
}

fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgi::{build_response, io, CgiTarget};
    use crate::config::types::ServerConfig;
    use crate::http::request::types::{HeaderMap, Method, Request, Version};
    use std::io::Write;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;
    use std::time::Duration;

    fn peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1234)
    }

    /// End-to-end spawn using `/bin/sh` as the "interpreter": the script emits
    /// a minimal CGI response, which we collect and parse into a 200.
    #[test]
    fn spawn_sh_script_emits_cgi_output() {
        let dir = format!("/tmp/cgi_exec_{}", unsafe { libc::getpid() });
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!("{dir}/hello.sh");
        {
            let mut f = std::fs::File::create(&script).unwrap();
            // CRLF header separator, no trailing newline on the body.
            f.write_all(b"printf 'Content-Type: text/plain\\r\\n\\r\\nhello-cgi'\n")
                .unwrap();
        }

        let target = CgiTarget {
            interpreter: "/bin/sh".into(),
            script_path: PathBuf::from(&script),
            script_dir: PathBuf::from(&dir),
        };
        let req = Request {
            method: Method::Get,
            path: "/hello.sh".into(),
            query: String::new(),
            version: Version::Http11,
            headers: HeaderMap::new(),
            body: Vec::new(),
        };

        let mut child = spawn(&target, &req, &ServerConfig::default(), 8080, peer())
            .expect("spawn should succeed");

        // No body: close stdin to deliver EOF.
        child.close_stdin();

        // Drain stdout until EOF (fds are non-blocking; spin briefly).
        let mut guard = 0;
        loop {
            match io::pump_stdout(&mut child) {
                io::StdoutOutcome::Eof => break,
                io::StdoutOutcome::Pending => {
                    std::thread::sleep(Duration::from_millis(2));
                    guard += 1;
                    assert!(guard < 1000, "CGI never reached EOF");
                }
                io::StdoutOutcome::Error => panic!("read error on CGI stdout"),
            }
        }

        unsafe {
            libc::close(child.stdout_fd);
            let mut status = 0;
            libc::waitpid(child.pid, &mut status, 0);
        }
        std::fs::remove_dir_all(&dir).ok();

        let resp = build_response(std::mem::take(&mut child.out_buf));
        assert_eq!(resp.status.code(), 200);
        assert_eq!(resp.body, b"hello-cgi");
    }

    /// Spawn with a body and have the script echo stdin back through the
    /// CGI body, exercising the stdin-pump path.
    #[test]
    fn spawn_sh_script_echoes_stdin_body() {
        let dir = format!("/tmp/cgi_exec_body_{}", unsafe { libc::getpid() });
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!("{dir}/echo.sh");
        {
            let mut f = std::fs::File::create(&script).unwrap();
            // Emit headers, then copy stdin to stdout.
            f.write_all(b"printf 'Content-Type: text/plain\\r\\n\\r\\n'\ncat\n")
                .unwrap();
        }

        let target = CgiTarget {
            interpreter: "/bin/sh".into(),
            script_path: PathBuf::from(&script),
            script_dir: PathBuf::from(&dir),
        };
        let req = Request {
            method: Method::Post,
            path: "/echo.sh".into(),
            query: String::new(),
            version: Version::Http11,
            headers: HeaderMap::new(),
            body: b"payload-123".to_vec(),
        };

        let mut child = spawn(&target, &req, &ServerConfig::default(), 8080, peer())
            .expect("spawn should succeed");

        // Pump stdin to completion, then stdout to EOF.
        let mut guard = 0;
        loop {
            match io::pump_stdin(&mut child) {
                io::StdinOutcome::Finished | io::StdinOutcome::Broken => break,
                io::StdinOutcome::Wrote | io::StdinOutcome::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    guard += 1;
                    assert!(guard < 1000, "stdin never finished");
                }
            }
        }

        guard = 0;
        loop {
            match io::pump_stdout(&mut child) {
                io::StdoutOutcome::Eof => break,
                io::StdoutOutcome::Pending => {
                    std::thread::sleep(Duration::from_millis(2));
                    guard += 1;
                    assert!(guard < 1000, "CGI never reached EOF");
                }
                io::StdoutOutcome::Error => panic!("read error on CGI stdout"),
            }
        }

        unsafe {
            libc::close(child.stdout_fd);
            let mut status = 0;
            libc::waitpid(child.pid, &mut status, 0);
        }
        std::fs::remove_dir_all(&dir).ok();

        let resp = build_response(std::mem::take(&mut child.out_buf));
        assert_eq!(resp.status.code(), 200);
        assert_eq!(resp.body, b"payload-123");
    }

    #[test]
    fn spawn_reports_error_for_bad_argument() {
        let target = CgiTarget {
            interpreter: "/bin/sh\0bad".into(), // interior NUL
            script_path: PathBuf::from("/tmp/whatever.sh"),
            script_dir: PathBuf::from("/tmp"),
        };
        let req = Request {
            method: Method::Get,
            path: "/x.sh".into(),
            query: String::new(),
            version: Version::Http11,
            headers: HeaderMap::new(),
            body: Vec::new(),
        };
        let err = spawn(&target, &req, &ServerConfig::default(), 8080, peer());
        assert!(matches!(err, Err(CgiError::BadArgument)));
    }
}