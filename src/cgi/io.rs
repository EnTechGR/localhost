//! Non-blocking I/O on the CGI pipes.
//!
//! Both functions operate on a [`CgiProcess`] and never block: they read or
//! write until the kernel reports `EAGAIN`/`EWOULDBLOCK`, then return so the
//! event loop can move on. The dispatcher calls them when `epoll` reports the
//! corresponding pipe fd is ready.
use crate::cgi::CgiProcess;

/// Bytes read per `read` syscall from the CGI stdout pipe.
const READ_CHUNK: usize = 8192;

// ---------------------------------------------------------------------------
// stdin (request body → child)
// ---------------------------------------------------------------------------

/// Result of pumping request-body bytes into the child's stdin.
#[derive(Debug)]
pub enum StdinOutcome {
    /// Wrote some bytes; the pipe then signalled it was full. More remain —
    /// wait for the next `EPOLLOUT`.
    Wrote,
    /// The full body has been written and stdin closed (child gets EOF).
    Finished,
    /// The pipe was immediately full; nothing written this round. Wait for
    /// the next `EPOLLOUT`. (Distinguished from `Wrote` only for clarity.)
    WouldBlock,
    /// The child closed its read end (`EPIPE`) or a fatal error occurred.
    /// stdin has been closed; stop pumping. The child may still produce
    /// output, so this is not necessarily a request failure.
    Broken,
}

/// Write as much of the remaining request body as the pipe will accept.
///
/// Loops until the pipe reports `EAGAIN`, the body is exhausted (then closes
/// stdin to deliver EOF), or the child goes away.
pub fn pump_stdin(process: &mut CgiProcess) -> StdinOutcome {
    let fd = match process.stdin_fd {
        Some(fd) => fd,
        None => return StdinOutcome::Finished,
    };

    let mut wrote_any = false;

    loop {
        if process.body_cursor >= process.body.len() {
            // Removing the fd from epoll is the caller's job; closing here
            // delivers EOF to the child.
            process.close_stdin();
            return StdinOutcome::Finished;
        }

        let slice = &process.body[process.body_cursor..];
        let n = unsafe {
            libc::write(fd, slice.as_ptr() as *const libc::c_void, slice.len())
        };

        if n > 0 {
            process.body_cursor += n as usize;
            wrote_any = true;
        } else {
            let e = errno();
            if e == libc::EAGAIN || e == libc::EWOULDBLOCK {
                return if wrote_any {
                    StdinOutcome::Wrote
                } else {
                    StdinOutcome::WouldBlock
                };
            }
            // EPIPE (child closed stdin) or another fatal error.
            process.close_stdin();
            return StdinOutcome::Broken;
        }
    }
}

// ---------------------------------------------------------------------------
// stdout (child → response buffer)
// ---------------------------------------------------------------------------

/// Result of pumping the child's stdout into `out_buf`.
#[derive(Debug)]
pub enum StdoutOutcome {
    /// Drained everything currently available; the child has not finished.
    /// Wait for the next `EPOLLIN`.
    Pending,
    /// EOF: the child closed stdout. `out_buf` now holds the complete output.
    Eof,
    /// A fatal read error occurred.
    Error,
}

/// Drain all bytes currently available on the CGI stdout pipe into `out_buf`.
pub fn pump_stdout(process: &mut CgiProcess) -> StdoutOutcome {
    let fd = process.stdout_fd;
    let mut tmp = [0u8; READ_CHUNK];

    loop {
        let n = unsafe {
            libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len())
        };

        if n > 0 {
            process.out_buf.extend_from_slice(&tmp[..n as usize]);
            // Keep reading until the pipe drains.
        } else if n == 0 {
            return StdoutOutcome::Eof;
        } else {
            let e = errno();
            if e == libc::EAGAIN || e == libc::EWOULDBLOCK {
                return StdoutOutcome::Pending;
            }
            return StdoutOutcome::Error;
        }
    }
}

fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}