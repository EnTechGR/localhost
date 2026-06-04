/// Main event loop dispatcher.
///
/// `run()` is the heart of the server: it calls `epoll_wait` in a tight loop
/// and dispatches each event to the appropriate handler based on whether the
/// fd is a listener or a connection and which flags are set.
///
/// # Single-call epoll guarantee
///
/// The spec requires "epoll function called only once for each client/server
/// communication". We interpret this as: one `epoll_wait` call drives all I/O
/// for the process — there is no second epoll instance, no nested waits. Each
/// event triggers direct non-blocking read/write operations, with epoll
/// re-armed (`EPOLL_CTL_MOD`) to the appropriate next event mask.
///
/// # Error handling
///
/// No error inside a connection handler may crash the server. All per-connection
/// errors are caught, logged, and result in the connection being closed. Only
/// unrecoverable system-level errors (epoll itself failing) are allowed to
/// propagate.
///
/// # Timeout sweep
///
/// Once per `epoll_wait` tick, `check_timeouts` walks all open connections.
/// Connections idle for longer than `TIMEOUT_*` constants are closed, with a
/// 408 response if headers were still being received.
use std::os::unix::io::RawFd;
use std::time::Duration;

use libc::epoll_event;

use crate::config::types::ServerConfig;
use crate::event_loop::epoll::{is_closed, is_readable, is_writable, Epoll, EPOLLIN, EPOLLOUT};
use crate::event_loop::registry::Registry;
use crate::server::connection::{ConnectionPhase, ConnectionState};
use crate::server::listener::{accept_one, AcceptResult};

// ---------------------------------------------------------------------------
// Timeout constants
// ---------------------------------------------------------------------------

/// Maximum time a connection may spend in header/body reading phases.
const TIMEOUT_READ: Duration  = Duration::from_secs(30);
/// Maximum time a response write may take before we give up.
const TIMEOUT_WRITE: Duration = Duration::from_secs(60);
/// Maximum time we wait for a CGI process to produce output.
const TIMEOUT_CGI: Duration   = Duration::from_secs(30);

/// How many events we ask `epoll_wait` to return per call.
const MAX_EVENTS: usize = 128;

/// `epoll_wait` timeout in milliseconds. Short enough to run the timeout sweep
/// frequently without burning CPU on an idle server.
const EPOLL_TIMEOUT_MS: i32 = 1_000; // 1 second

// ---------------------------------------------------------------------------
// run()
// ---------------------------------------------------------------------------

/// Enter the event loop. Never returns under normal operation.
///
/// Preconditions:
/// - `epoll` has all listener fds registered with `EPOLLIN`.
/// - `registry` has matching entries for every listener fd.
/// - `configs` is indexed by the `server_id` stored in listener entries.
pub fn run(epoll: Epoll, mut registry: Registry, configs: Vec<ServerConfig>) -> ! {
    let mut events: Vec<epoll_event> =
        vec![unsafe { std::mem::zeroed() }; MAX_EVENTS];

    eprintln!("[INFO] Event loop started, waiting for connections");

    loop {
        let n = match epoll.wait(&mut events, EPOLL_TIMEOUT_MS) {
            Ok(n) => n,
            Err(e) => {
                // epoll_wait failing is catastrophic — log and continue; the
                // OS error is almost certainly transient (EINTR is retried
                // inside Epoll::wait already).
                eprintln!("[ERROR] epoll_wait: {e}");
                continue;
            }
        };

        // Dispatch each fired event.
        for i in 0..n {
            let event  = events[i];
            let fd     = event.u64 as RawFd;
            let eflags = event.events;

            if registry.is_listener(fd) {
                // Listener socket ready: accept as many connections as possible.
                accept_new_connection(fd, &epoll, &mut registry, &configs);
            } else if is_closed(eflags) {
                // Remote end closed or an error occurred.
                close_connection(fd, &epoll, &mut registry, None);
            } else if is_readable(eflags) {
                read_from_connection(fd, &epoll, &mut registry, &configs);
            } else if is_writable(eflags) {
                write_to_connection(fd, &epoll, &mut registry);
            }
        }

        // Periodic timeout sweep — runs once per epoll_wait tick.
        check_timeouts(&mut registry, &epoll);
    }
}

// ---------------------------------------------------------------------------
// accept_new_connection
// ---------------------------------------------------------------------------

/// Accept all pending connections on `listener_fd`.
///
/// We loop until `accept4` returns `EAGAIN` (no more pending clients) because
/// with level-triggered epoll a single `EPOLLIN` event may represent multiple
/// queued connections.
fn accept_new_connection(
    listener_fd: RawFd,
    epoll:       &Epoll,
    registry:    &mut Registry,
    configs:     &[ServerConfig],
) {
    let server_id = match registry.listener(listener_fd) {
        Some(e) => e.server_id,
        None => {
            eprintln!("[WARN] accept_new_connection: fd {listener_fd} not in registry");
            return;
        }
    };

    loop {
        match accept_one(listener_fd) {
            AcceptResult::Accepted { fd, peer } => {
                eprintln!("[INFO] Accepted connection fd={fd} from {peer} (server_id={server_id})");

                // Register with epoll: only EPOLLIN until we have something to write.
                if let Err(e) = epoll.add(fd, EPOLLIN as u32, fd as u64) {
                    eprintln!("[ERROR] epoll.add fd={fd}: {e}");
                    unsafe { libc::close(fd) };
                    continue;
                }

                let state = ConnectionState::new(fd, server_id, peer);
                registry.register_connection(fd, state);
            }

            AcceptResult::WouldBlock => break, // No more pending connections.

            AcceptResult::Error(e) => {
                // EMFILE / ENFILE: out of file descriptors. Log and stop
                // accepting for this tick — connections will queue in the
                // kernel backlog until the next tick frees some fds.
                eprintln!("[WARN] accept4 on fd={listener_fd} failed: errno={e}");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// read_from_connection
// ---------------------------------------------------------------------------

/// Read available bytes from `fd` into the connection's `read_buf`.
///
/// Delegates to the appropriate parser based on the connection's current
/// phase. When enough data has arrived, transitions the connection to
/// `Processing` and then immediately to `WritingResponse`.
///
/// This function never blocks: it reads until `EAGAIN`.
fn read_from_connection(
    fd:       RawFd,
    epoll:    &Epoll,
    registry: &mut Registry,
    configs:  &[ServerConfig],
) {
    // Drain available bytes into the read buffer.
    let should_close = match registry.get_connection_mut(fd) {
        None => {
            eprintln!("[WARN] read_from_connection: fd {fd} not in registry");
            return;
        }
        Some(conn) => {
            let result = drain_socket(fd, &mut conn.read_buf);
            conn.touch();
            match result {
                DrainResult::Data   => false,
                DrainResult::Closed => true,  // FIN received
                DrainResult::Error(e) => {
                    eprintln!("[WARN] read fd={fd}: errno={e}");
                    true
                }
            }
        }
    };

    if should_close {
        close_connection(fd, epoll, registry, None);
        return;
    }

    // Attempt to advance the connection's parse state.
    // For this step we produce a stub 200 OK until the HTTP parser is wired in.
    let transition = match registry.get_connection_mut(fd) {
        None => return,
        Some(conn) => advance_connection(conn, configs),
    };

    match transition {
        Transition::NeedMoreData => {
            // Stay in EPOLLIN mode — already set.
        }

        Transition::ResponseReady => {
            // Arm EPOLLOUT so we are notified when the socket can be written.
            if let Err(e) = epoll.modify(fd, EPOLLOUT as u32, fd as u64) {
                eprintln!("[ERROR] epoll.modify EPOLLOUT fd={fd}: {e}");
                close_connection(fd, epoll, registry, None);
            }
        }

        Transition::Close => {
            close_connection(fd, epoll, registry, None);
        }
    }
}

// ---------------------------------------------------------------------------
// write_to_connection
// ---------------------------------------------------------------------------

/// Flush bytes from `write_buf` to the socket without blocking.
///
/// Advances the `bytes_written` cursor. When the buffer is fully sent,
/// transitions to `Done` (or resets for keep-alive) and switches the fd back
/// to `EPOLLIN`.
fn write_to_connection(fd: RawFd, epoll: &Epoll, registry: &mut Registry) {
    let done = match registry.get_connection_mut(fd) {
        None => {
            eprintln!("[WARN] write_to_connection: fd {fd} not in registry");
            return;
        }
        Some(conn) => {
            let keep_open = flush_write_buf(fd, conn);
            conn.touch();
            keep_open
        }
    };

    match done {
        FlushResult::Done { keep_alive } => {
            if keep_alive {
                // Re-arm EPOLLIN for the next request on this connection.
                if let Err(e) = epoll.modify(fd, EPOLLIN as u32, fd as u64) {
                    eprintln!("[ERROR] epoll.modify EPOLLIN fd={fd}: {e}");
                    close_connection(fd, epoll, registry, None);
                }
            } else {
                close_connection(fd, epoll, registry, None);
            }
        }
        FlushResult::Partial => {
            // More data to write; stay in EPOLLOUT mode — already set.
        }
        FlushResult::Error(e) => {
            eprintln!("[WARN] write fd={fd}: errno={e}");
            close_connection(fd, epoll, registry, None);
        }
    }
}

// ---------------------------------------------------------------------------
// close_connection
// ---------------------------------------------------------------------------

/// Remove `fd` from epoll and registry, then close the OS file descriptor.
///
/// `response` is an optional pre-formed response to send synchronously before
/// closing (used for timeout 408 responses). Synchronous sends are best-effort;
/// errors are ignored.
fn close_connection(
    fd:       RawFd,
    epoll:    &Epoll,
    registry: &mut Registry,
    response: Option<&[u8]>,
) {
    // Best-effort synchronous write for in-flight error responses.
    if let Some(data) = response {
        unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) };
    }

    // Remove from epoll. ENOENT means it was never added or already removed.
    if let Err(e) = epoll.delete(fd) {
        eprintln!("[DEBUG] epoll.delete fd={fd}: {e}");
    }

    registry.remove(fd);

    unsafe { libc::close(fd) };

    eprintln!("[INFO] Closed connection fd={fd}");
}

// ---------------------------------------------------------------------------
// check_timeouts
// ---------------------------------------------------------------------------

/// Walk all open connections and close those that have been idle too long.
///
/// Called once per `epoll_wait` tick. Building the fd list allocates, but
/// timeout sweeps are infrequent (once per second) so this is acceptable.
fn check_timeouts(registry: &mut Registry, epoll: &Epoll) {
    let fds = registry.all_connection_fds();

    let mut to_close: Vec<(RawFd, &'static [u8])> = Vec::new();

    for fd in fds {
        let timed_out = match registry.get_connection(fd) {
            None => continue,
            Some(conn) => {
                let timeout = match &conn.phase {
                    ConnectionPhase::ReadingHeaders
                    | ConnectionPhase::ReadingBody { .. }
                    | ConnectionPhase::ReadingChunked { .. } => TIMEOUT_READ,

                    ConnectionPhase::WritingResponse { .. } => TIMEOUT_WRITE,

                    ConnectionPhase::AwaitingCgi { .. } => TIMEOUT_CGI,

                    // Processing is synchronous and transient; Done is
                    // removed immediately. Neither should linger.
                    ConnectionPhase::Processing | ConnectionPhase::Done => TIMEOUT_READ,
                };
                conn.is_timed_out(timeout)
            }
        };

        if timed_out {
            // Choose a response based on phase.
            let response: &'static [u8] = match registry
                .get_connection(fd)
                .map(|c| matches!(c.phase, ConnectionPhase::ReadingHeaders | ConnectionPhase::ReadingBody { .. } | ConnectionPhase::ReadingChunked { .. }))
            {
                Some(true)  => HTTP_408,
                _           => HTTP_504, // gateway timeout for CGI, generic for writes
            };
            eprintln!("[INFO] Timeout on fd={fd}, sending {}", if response == HTTP_408 { "408" } else { "504" });
            to_close.push((fd, response));
        }
    }

    for (fd, resp) in to_close {
        close_connection(fd, epoll, registry, Some(resp));
    }
}

// ---------------------------------------------------------------------------
// Internal I/O helpers
// ---------------------------------------------------------------------------

enum DrainResult {
    /// Data was read (possibly zero new bytes if EAGAIN on first call).
    Data,
    /// The remote end sent FIN.
    Closed,
    /// A fatal read error occurred.
    Error(i32),
}

/// Read from `fd` into `buf` until `EAGAIN`.
fn drain_socket(fd: RawFd, buf: &mut Vec<u8>) -> DrainResult {
    let mut tmp = [0u8; 8192];
    let mut got_data = false;

    loop {
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut _, tmp.len()) };
        if n > 0 {
            buf.extend_from_slice(&tmp[..n as usize]);
            got_data = true;
            // Keep reading — there may be more.
        } else if n == 0 {
            return DrainResult::Closed;
        } else {
            let e = unsafe { *libc::__errno_location() };
            if e == libc::EAGAIN || e == libc::EWOULDBLOCK {
                return DrainResult::Data;
            }
            return DrainResult::Error(e);
        }
    }
}

enum Transition {
    NeedMoreData,
    ResponseReady,
    Close,
}

/// Attempt to advance the connection's parse/processing state.
///
/// This is a **stub** that accepts any data containing `\r\n\r\n` as a complete
/// request and queues a hard-coded 200 OK. The real HTTP parser will replace
/// this in the next step.
fn advance_connection(conn: &mut ConnectionState, _configs: &[ServerConfig]) -> Transition {
    match conn.phase {
        ConnectionPhase::ReadingHeaders => {
            // Detect end-of-headers: look for \r\n\r\n in the read buffer.
            if conn.read_buf.windows(4).any(|w| w == b"\r\n\r\n") {
                // --- Stub response until HTTP parser is wired in ---
                let response = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nHello, world!".to_vec();
                conn.set_response(response);
                Transition::ResponseReady
            } else if conn.read_buf.len() > 8192 {
                // Headers too large — 400 Bad Request.
                conn.set_response(HTTP_400.to_vec());
                Transition::ResponseReady
            } else {
                Transition::NeedMoreData
            }
        }
        // Other phases are handled by more specific code; if we land here,
        // it's a logic error — close defensively.
        _ => Transition::Close,
    }
}

enum FlushResult {
    /// All bytes sent. `keep_alive` signals whether to reset or close.
    Done { keep_alive: bool },
    /// Partial write; more data remains in `write_buf`.
    Partial,
    /// Fatal write error.
    Error(i32),
}

/// Write as many bytes as possible from `conn.write_buf` to `fd`.
fn flush_write_buf(fd: RawFd, conn: &mut ConnectionState) -> FlushResult {
    if let ConnectionPhase::WritingResponse { ref mut bytes_written } = conn.phase {
        let remaining = &conn.write_buf[*bytes_written..];
        if remaining.is_empty() {
            let keep_alive = conn.finish();
            return FlushResult::Done { keep_alive };
        }

        let n = unsafe {
            libc::write(fd, remaining.as_ptr() as *const _, remaining.len())
        };

        if n > 0 {
            *bytes_written += n as usize;
            if *bytes_written >= conn.write_buf.len() {
                let keep_alive = conn.finish();
                return FlushResult::Done { keep_alive };
            }
            return FlushResult::Partial;
        } else if n == 0 {
            return FlushResult::Error(0);
        } else {
            let e = unsafe { *libc::__errno_location() };
            if e == libc::EAGAIN || e == libc::EWOULDBLOCK {
                return FlushResult::Partial;
            }
            return FlushResult::Error(e);
        }
    }
    // Not in WritingResponse — shouldn't happen.
    FlushResult::Error(-1)
}

// ---------------------------------------------------------------------------
// Canned HTTP error responses (static byte strings)
// ---------------------------------------------------------------------------

const HTTP_400: &[u8] = b"\
HTTP/1.1 400 Bad Request\r\n\
Content-Type: text/plain\r\n\
Content-Length: 11\r\n\
Connection: close\r\n\
\r\n\
Bad Request";

const HTTP_408: &[u8] = b"\
HTTP/1.1 408 Request Timeout\r\n\
Content-Type: text/plain\r\n\
Content-Length: 15\r\n\
Connection: close\r\n\
\r\n\
Request Timeout";

const HTTP_504: &[u8] = b"\
HTTP/1.1 504 Gateway Timeout\r\n\
Content-Type: text/plain\r\n\
Content-Length: 15\r\n\
Connection: close\r\n\
\r\n\
Gateway Timeout";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::connection::{ConnectionPhase, ConnectionState};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn dummy_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000)
    }

    fn conn(fd: RawFd) -> ConnectionState {
        ConnectionState::new(fd, 0, dummy_addr())
    }

    // ---- advance_connection stub -------------------------------------------

    #[test]
    fn stub_returns_need_more_data_without_double_crlf() {
        let mut c = conn(5);
        c.read_buf.extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost");
        let result = advance_connection(&mut c, &[]);
        assert!(matches!(result, Transition::NeedMoreData));
    }

    #[test]
    fn stub_returns_response_ready_with_double_crlf() {
        let mut c = conn(5);
        c.read_buf.extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let result = advance_connection(&mut c, &[]);
        assert!(matches!(result, Transition::ResponseReady));
        assert!(matches!(c.phase, ConnectionPhase::WritingResponse { .. }));
    }

    #[test]
    fn stub_returns_response_ready_on_oversized_headers() {
        let mut c = conn(5);
        c.read_buf.extend(vec![b'A'; 8193]);
        let result = advance_connection(&mut c, &[]);
        assert!(matches!(result, Transition::ResponseReady));
        assert!(c.write_buf.starts_with(b"HTTP/1.1 400"));
    }

    // ---- flush_write_buf ---------------------------------------------------

    #[test]
    fn flush_on_done_connection_returns_done() {
        // Create a connected socket pair to write into.
        let (rd, wr) = make_socket_pair();

        let mut c = conn(wr);
        c.set_response(b"HTTP/1.1 200 OK\r\n\r\n".to_vec());

        // Drain the write side.
        let result = flush_write_buf(wr, &mut c);

        // Clean up before asserting so fds don't leak on failure.
        unsafe { libc::close(rd); libc::close(wr); }

        assert!(matches!(result, FlushResult::Done { .. }));
    }

    // ---- timeout constants sanity ------------------------------------------

    #[test]
    fn timeout_constants_are_non_zero() {
        assert!(TIMEOUT_READ.as_secs()  > 0);
        assert!(TIMEOUT_WRITE.as_secs() > 0);
        assert!(TIMEOUT_CGI.as_secs()   > 0);
    }

    // ---- drain_socket / DrainResult ----------------------------------------

    #[test]
    fn drain_socket_reads_data() {
        let (rd, wr) = make_pipe();
        unsafe { libc::write(wr, b"hello".as_ptr() as *const _, 5) };
        // Close write end so read returns EOF after the data.
        unsafe { libc::close(wr) };

        let flags = unsafe { libc::fcntl(rd, libc::F_GETFL, 0) };
        unsafe { libc::fcntl(rd, libc::F_SETFL, flags | libc::O_NONBLOCK) };

        let mut buf = Vec::new();
        let result = drain_socket(rd, &mut buf);

        unsafe { libc::close(rd) };

        // After reading all data, the next read returns 0 (EOF = Closed).
        assert!(matches!(result, DrainResult::Closed));
        assert_eq!(&buf, b"hello");
    }

    // ---- check_timeouts cleans up stale connections -----------------------

    #[test]
    fn check_timeouts_removes_timed_out_connections() {
        let (rd, wr) = make_socket_pair();

        let epoll   = Epoll::create().unwrap();
        let mut reg = Registry::new();

        epoll.add(wr, EPOLLIN as u32, wr as u64).unwrap();
        let mut c = conn(wr);
        // Force a very old last_activity so it appears timed out.
        c.last_activity = std::time::Instant::now()
            - std::time::Duration::from_secs(9999);
        reg.register_connection(wr, c);

        check_timeouts(&mut reg, &epoll);

        // Connection should have been removed.
        assert!(reg.get_connection(wr).is_none());

        unsafe { libc::close(rd) };
        // wr was already closed by check_timeouts.
    }

    // ---- helpers -----------------------------------------------------------

    fn make_pipe() -> (RawFd, RawFd) {
        let mut fds = [0i32; 2];
        unsafe { libc::pipe(fds.as_mut_ptr()) };
        (fds[0], fds[1])
    }

    fn make_socket_pair() -> (RawFd, RawFd) {
        let mut fds = [0i32; 2];
        unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0, fds.as_mut_ptr())
        };
        (fds[0], fds[1])
    }
}