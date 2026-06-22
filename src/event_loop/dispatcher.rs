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

use libc::epoll_event;

use crate::config::types::ServerConfig;
use crate::event_loop::epoll::{is_closed, is_readable, is_writable, Epoll, EPOLLIN, EPOLLOUT};
use crate::event_loop::registry::Registry;
use crate::http::request::body::{read_body_chunked, read_body_unchunked, BodyResult, ChunkedResult};
use crate::http::request::parser::{parse_request_head, ParseResult};
use crate::http::response::writer::{serialize, write_nonblocking, WriteResult};
use crate::router::handler::dispatch;
use crate::router::matcher::{match_route, select_server};
use crate::server::connection::{ConnectionPhase, ConnectionState};
use crate::server::listener::{accept_one, AcceptResult};
use crate::server::timeout::{canned_response, timeout_for_phase, TimeoutClass};
use crate::cgi::{self, io::{StdinOutcome, StdoutOutcome}};
use crate::router::handler::DispatchOutcome;

/// How many events we ask `epoll_wait` to return per call.
const MAX_EVENTS: usize = 128;

/// `epoll_wait` timeout in milliseconds. Short enough to run the timeout sweep
/// frequently without burning CPU on an idle server.
const EPOLL_TIMEOUT_MS: i32 = 1_000; // 1 second

/// Soft cap on simultaneous connections. Prevents fd exhaustion under siege.
/// The OS default ulimit is typically 1024; we stay well under that.
const MAX_CONNECTIONS: usize = 512;

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

    eprintln!(
        "[INFO] Event loop started: {} listener(s) registered",
        registry.listener_count()
    );
    for (fd, entry) in registry.listeners() {
        eprintln!(
            "[INFO]   listener fd={fd} (server_id={}, port={})",
            entry.server_id, entry.port
        );
    }

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
                accept_new_connection(fd, &epoll, &mut registry);
            } else if registry.is_cgi_fd(fd) {
                handle_cgi_event(fd, eflags, &epoll, &mut registry);
            } else if is_closed(eflags) {
                close_connection(fd, &epoll, &mut registry, None);
            } else if is_readable(eflags) {
                read_from_connection(fd, &epoll, &mut registry, &configs);
            } else if is_writable(eflags) {
                write_to_connection(fd, &epoll, &mut registry);
            }
        }

        // Periodic timeout sweep — runs once per epoll_wait tick.
        check_timeouts(&mut registry, &epoll);
        reap_children();
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
) {
    let (server_id, local_port) = match registry.listener(listener_fd) {
        Some(e) => (e.server_id, e.port),
        None => {
            eprintln!("[WARN] accept_new_connection: fd {listener_fd} not in registry");
            return;
        }
    };

    loop {
        // Back-pressure: stop accepting when near fd limit.
        if registry.connection_count() >= MAX_CONNECTIONS {
            break;
        }
        match accept_one(listener_fd) {
            AcceptResult::Accepted { fd, peer } => {
                eprintln!("[INFO] Accepted connection fd={fd} from {peer} (server_id={server_id}, port={local_port})");

                if let Err(e) = epoll.add(fd, EPOLLIN as u32, fd as u64) {
                    eprintln!("[ERROR] epoll.add fd={fd}: {e}");
                    unsafe { libc::close(fd) };
                    continue;
                }

                let state = ConnectionState::new(fd, server_id, local_port, peer);
                registry.register_connection(fd, state);
            }

            AcceptResult::WouldBlock => break,

            AcceptResult::Error(e) => {
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

    // Advance the connection through parse → route → respond.
    let transition = match registry.get_connection_mut(fd) {
        None => return,
        Some(conn) => advance_connection(conn, configs),
    };

    match transition {
        Transition::NeedMoreData => {
            // Stay in EPOLLIN mode — already set.
        }

        Transition::ResponseReady => {
            if let Err(e) = epoll.modify(fd, EPOLLOUT as u32, fd as u64) {
                eprintln!("[ERROR] epoll.modify EPOLLOUT fd={fd}: {e}");
                close_connection(fd, epoll, registry, None);
            }
        }

        Transition::Close => {
            close_connection(fd, epoll, registry, None);
        }

        Transition::CgiStarted { stdout_fd, stdin_fd } => {
            if let Err(e) = epoll.add(stdout_fd, EPOLLIN as u32, stdout_fd as u64) {
                eprintln!("[ERROR] epoll.add cgi stdout fd={stdout_fd}: {e}");
                close_connection(fd, epoll, registry, None);
                return;
            }
            registry.register_cgi_fd(stdout_fd, fd);

            if let Some(in_fd) = stdin_fd {
                if let Err(e) = epoll.add(in_fd, EPOLLOUT as u32, in_fd as u64) {
                    eprintln!("[ERROR] epoll.add cgi stdin fd={in_fd}: {e}");
                    // Not fatal to the exchange: stdout alone can still drive
                    // it to completion if the script ignores stdin.
                } else {
                    registry.register_cgi_fd(in_fd, fd);
                }
            }

            // Disarm the client socket itself — only the pipe fds matter
            // until the CGI exchange finishes. EPOLLRDHUP is still added
            // automatically by Epoll::modify, so a client disconnect mid-CGI
            // is still detected and cleaned up via close_connection.
            if let Err(e) = epoll.modify(fd, 0, fd as u64) {
                eprintln!("[ERROR] epoll.modify disarm fd={fd}: {e}");
                close_connection(fd, epoll, registry, None);
            }
        }
    }
}


// ---------------------------------------------------------------------------
// CGI pipe events
// ---------------------------------------------------------------------------

/// Dispatch an epoll event on a CGI pipe fd to the stdin- or stdout-side
/// handler, based on which pipe it actually is on the owning connection.
fn handle_cgi_event(pipe_fd: RawFd, eflags: u32, epoll: &Epoll, registry: &mut Registry) {
    let owner_fd = match registry.cgi_owner(pipe_fd) {
        Some(fd) => fd,
        None => {
            eprintln!("[WARN] cgi event on fd {pipe_fd} with no owner; dropping");
            let _ = epoll.delete(pipe_fd);
            unsafe { libc::close(pipe_fd) };
            return;
        }
    };

    let is_stdout = match registry.get_connection_mut(owner_fd).and_then(|c| c.cgi.as_ref()) {
        Some(p) => p.stdout_fd == pipe_fd,
        None => {
            // Owning connection (or its CGI state) is already gone.
            let _ = epoll.delete(pipe_fd);
            unsafe { libc::close(pipe_fd) };
            registry.unregister_cgi_fd(pipe_fd);
            return;
        }
    };

    if is_stdout {
        handle_cgi_stdout(owner_fd, eflags, epoll, registry);
    } else {
        handle_cgi_stdin(pipe_fd, owner_fd, epoll, registry);
    }
}

/// Pump available output from the CGI child into `out_buf`. On EOF or a
/// fatal read error, finish the exchange and produce a response.
fn handle_cgi_stdout(owner_fd: RawFd, eflags: u32, epoll: &Epoll, registry: &mut Registry) {
    let outcome = match registry.get_connection_mut(owner_fd) {
        None => return,
        Some(conn) => {
            conn.touch();
            match conn.cgi.as_mut() {
                Some(process) => cgi::io::pump_stdout(process),
                None => return,
            }
        }
    };

    match outcome {
        StdoutOutcome::Pending => {
            // is_closed can fire alongside a final readable event (the child
            // exited right after writing its last bytes); treat that as EOF
            // too rather than waiting for a second tick that may never come.
            if is_closed(eflags) {
                finish_cgi(owner_fd, epoll, registry, false);
            }
        }
        StdoutOutcome::Eof => {
            finish_cgi(owner_fd, epoll, registry, false);
        }
        StdoutOutcome::Error => {
            finish_cgi(owner_fd, epoll, registry, true);
        }
    }
}

/// Pump request-body bytes into the CGI child's stdin.
fn handle_cgi_stdin(pipe_fd: RawFd, owner_fd: RawFd, epoll: &Epoll, registry: &mut Registry) {
    let outcome = match registry.get_connection_mut(owner_fd) {
        None => return,
        Some(conn) => {
            conn.touch();
            match conn.cgi.as_mut() {
                Some(process) => cgi::io::pump_stdin(process),
                None => return,
            }
        }
    };

    match outcome {
        StdinOutcome::Wrote | StdinOutcome::WouldBlock => {
            // Stay armed for the next EPOLLOUT.
        }
        StdinOutcome::Finished | StdinOutcome::Broken => {
            // pump_stdin already closed the fd (delivering EOF, or because
            // the child went away) — just stop tracking it.
            let _ = epoll.delete(pipe_fd);
            registry.unregister_cgi_fd(pipe_fd);
        }
    }
}

/// Tear down both CGI pipes for `owner_fd`'s connection, reap the child
/// (best-effort, non-blocking), and turn whatever was collected into a
/// response. `force_error` is set when stdout hit a fatal read error rather
/// than a clean EOF.
fn finish_cgi(owner_fd: RawFd, epoll: &Epoll, registry: &mut Registry, force_error: bool) {
    let process = match registry.get_connection_mut(owner_fd) {
        None => return,
        Some(conn) => conn.cgi.take(),
    };
    let mut process = match process {
        Some(p) => p,
        None => return,
    };

    let _ = epoll.delete(process.stdout_fd);
    registry.unregister_cgi_fd(process.stdout_fd);
    unsafe { libc::close(process.stdout_fd) };

    if let Some(stdin_fd) = process.stdin_fd.take() {
        let _ = epoll.delete(stdin_fd);
        registry.unregister_cgi_fd(stdin_fd);
        unsafe { libc::close(stdin_fd) };
    }

    // Best-effort, non-blocking reap. If the child hasn't exited yet (rare —
    // it just closed stdout but may still be cleaning up), the periodic
    // reap_children() sweep will collect it later; we never block here.
    unsafe { libc::waitpid(process.pid, std::ptr::null_mut(), libc::WNOHANG) };

    let mut response = if force_error || process.out_buf.is_empty() {
        let body = b"<html><body><h1>502 Bad Gateway</h1>\
                     <p>The CGI script produced no valid response.</p>\
                     </body></html>".to_vec();
        crate::http::response::builder::error(502, body)
    } else {
        cgi::build_response(std::mem::take(&mut process.out_buf))
    };

    let conn = match registry.get_connection_mut(owner_fd) {
        None => return,
        Some(c) => c,
    };

    if !conn.keep_alive {
        response.headers.set("Connection", "close");
    }
    conn.set_response(serialize(&response));

    if let Err(e) = epoll.modify(owner_fd, EPOLLOUT as u32, owner_fd as u64) {
        eprintln!("[ERROR] epoll.modify EPOLLOUT fd={owner_fd}: {e}");
        close_connection(owner_fd, epoll, registry, None);
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
    if let Some(data) = response {
        unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) };
    }

    // If a CGI exchange was in flight for this connection, tear it down
    // first: kill the child, close both pipes, and stop tracking them —
    // otherwise they'd leak (fd exhaustion under siege) and the child would
    // become a zombie once it exits.
    if let Some(mut process) = registry.get_connection_mut(fd).and_then(|c| c.cgi.take()) {
        unsafe { libc::kill(process.pid, libc::SIGKILL) };

        let _ = epoll.delete(process.stdout_fd);
        registry.unregister_cgi_fd(process.stdout_fd);
        unsafe { libc::close(process.stdout_fd) };

        if let Some(stdin_fd) = process.stdin_fd.take() {
            let _ = epoll.delete(stdin_fd);
            registry.unregister_cgi_fd(stdin_fd);
            unsafe { libc::close(stdin_fd) };
        }

        unsafe { libc::waitpid(process.pid, std::ptr::null_mut(), libc::WNOHANG) };
    }

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
/// Called once per `epoll_wait` tick. Uses `timeout_for_phase` from
/// `server::timeout` so phase-to-timeout mapping stays in one place.
fn check_timeouts(registry: &mut Registry, epoll: &Epoll) {
    let fds = registry.all_connection_fds();
    let mut to_close: Vec<(RawFd, &'static [u8])> = Vec::new();

    for fd in fds {
        let verdict = match registry.get_connection(fd) {
            None => continue,
            Some(conn) => {
                match timeout_for_phase(&conn.phase) {
                    None                     => None, // Done — no timeout needed
                    Some((dur, class)) => {
                        if conn.is_timed_out(dur) {
                            Some(class)
                        } else {
                            None
                        }
                    }
                }
            }
        };

        if let Some(class) = verdict {
            let label = match class {
                TimeoutClass::RequestTimeout => "408",
                TimeoutClass::GatewayTimeout => "504",
            };
            eprintln!("[INFO] Timeout ({label}) on fd={fd}");
            to_close.push((fd, canned_response(class)));
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

    loop {
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut _, tmp.len()) };
        if n > 0 {
            buf.extend_from_slice(&tmp[..n as usize]);
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
    /// CGI child spawned; `stdout_fd` must be registered for `EPOLLIN` and
    /// `stdin_fd` (if `Some`) for `EPOLLOUT`. Carried out of `process_request`
    /// as plain fds (not the `CgiProcess` itself) because registering them
    /// needs `&mut Registry`, which can't be borrowed while `conn` is.
    CgiStarted { stdout_fd: RawFd, stdin_fd: Option<RawFd> },
}

/// Advance the connection through its parse → route → respond pipeline.
///
/// All phase transitions use the methods on `ConnectionState`; the
/// dispatcher does not manipulate `conn.phase` directly.
fn advance_connection(conn: &mut ConnectionState, configs: &[ServerConfig]) -> Transition {
    loop {
        match &conn.phase {
            // ----------------------------------------------------------------
            // Phase 1: parse request headers
            // ----------------------------------------------------------------
            ConnectionPhase::ReadingHeaders => {
                match parse_request_head(&conn.read_buf) {
                    ParseResult::Incomplete => return Transition::NeedMoreData,

                    ParseResult::Error(e) => {
                        eprintln!("[WARN] parse error fd={}: {e}", conn.fd);
                        conn.set_response(HTTP_400.to_vec());
                        return Transition::ResponseReady;
                    }

                    ParseResult::Complete(req, consumed) => {
                        let body_so_far = conn.read_buf[consumed..].to_vec();

                        if req.is_chunked() {
                            conn.begin_reading_chunked(req, body_so_far);
                            continue;
                        } else if let Some(cl) = req.content_length() {
                            if cl == 0 {
                                conn.keep_alive = req.is_keep_alive();
                                conn.request = Some(req);
                                conn.read_buf.clear();
                                conn.phase = ConnectionPhase::Processing;
                                return process_request(conn, configs);
                            }
                            conn.begin_reading_body(req, cl, body_so_far);
                            continue;
                        } else {
                            conn.keep_alive = req.is_keep_alive();
                            conn.request = Some(req);
                            conn.read_buf.clear();
                            conn.phase = ConnectionPhase::Processing;
                            return process_request(conn, configs);
                        }
                    }
                }
            }

            // ----------------------------------------------------------------
            // Phase 2a: fixed-length body
            // ----------------------------------------------------------------
            ConnectionPhase::ReadingBody { expected, .. } => {
                let expected = *expected;
                match read_body_unchunked(&conn.read_buf, expected) {
                    BodyResult::NeedsMore { .. } => return Transition::NeedMoreData,
                    BodyResult::Complete(body) => {
                        conn.complete_body(body);
                        return process_request(conn, configs);
                    }
                }
            }

            // ----------------------------------------------------------------
            // Phase 2b: chunked body
            // ----------------------------------------------------------------
            ConnectionPhase::ReadingChunked { assembled } => {
                let assembled = assembled.clone();
                match read_body_chunked(&assembled) {
                    ChunkedResult::NeedsMore => return Transition::NeedMoreData,
                    ChunkedResult::Error(e) => {
                        eprintln!("[WARN] chunked decode error fd={}: {e}", conn.fd);
                        conn.set_response(HTTP_400.to_vec());
                        return Transition::ResponseReady;
                    }
                    ChunkedResult::Complete(body, _) => {
                        conn.complete_body(body);
                        return process_request(conn, configs);
                    }
                }
            }

            ConnectionPhase::Processing
            | ConnectionPhase::WritingResponse { .. }
            | ConnectionPhase::AwaitingCgi { .. }
            | ConnectionPhase::Done => return Transition::Close,
        }
    }
}

/// Dispatch the stored `Request` and serialise the response.
fn process_request(conn: &mut ConnectionState, configs: &[ServerConfig]) -> Transition {
    if configs.is_empty() {
        conn.set_response(HTTP_500.to_vec());
        return Transition::ResponseReady;
    }

    let req = match conn.request.take() {
        Some(r) => r,
        None => {
            conn.set_response(HTTP_500.to_vec());
            return Transition::ResponseReady;
        }
    };

    let server = select_server(req.host(), conn.local_port, configs);

    let route = match match_route(&req.path, &server.routes) {
        Some(r) => r,
        None => {
            let page  = server.error_page(404)
                .and_then(|p| std::fs::read_to_string(p).ok());
            let mut resp = crate::http::response::builder::not_found(page.as_deref());
            if !conn.keep_alive {
                resp.headers.set("Connection", "close");
            }
            let bytes = serialize(&resp);
            conn.set_response(bytes);
            return Transition::ResponseReady;
        }
    };

    match dispatch(&req, route, server) {
        DispatchOutcome::Response(mut response) => {
            if !conn.keep_alive {
                response.headers.set("Connection", "close");
            }
            let bytes = if req.method == crate::config::types::Method::Head {
                crate::http::response::writer::serialize_head_response(&response)
            } else {
                serialize(&response)
            };
            conn.set_response(bytes);
            Transition::ResponseReady
        }

        DispatchOutcome::StartCgi(target) => {
            match cgi::executor::spawn(&target, &req, server, conn.local_port, conn.peer_addr) {
                Ok(mut process) => {
                    // No body to send (GET, or POST with an empty body):
                    // close stdin immediately so the child sees EOF and we
                    // never register a stdin fd that would sit idle forever.
                    if process.body.is_empty() {
                        process.close_stdin();
                    }
                    let stdout_fd = process.stdout_fd;
                    let stdin_fd  = process.stdin_fd;

                    conn.phase = ConnectionPhase::AwaitingCgi {
                        child_pid: process.pid,
                        pipe_fd:   stdout_fd,
                    };
                    conn.cgi = Some(process);

                    Transition::CgiStarted { stdout_fd, stdin_fd }
                }
                Err(e) => {
                    eprintln!("[WARN] CGI spawn failed: {e}");
                    conn.set_response(HTTP_502.to_vec());
                    Transition::ResponseReady
                }
            }
        }
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
        let total = conn.write_buf.len();
        let offset = *bytes_written;

        if offset >= total {
            let keep_alive = conn.finish();
            return FlushResult::Done { keep_alive };
        }

        match write_nonblocking(fd, &conn.write_buf, offset) {
            WriteResult::BytesWritten(n) => {
                *bytes_written += n;
                if *bytes_written >= total {
                    let keep_alive = conn.finish();
                    FlushResult::Done { keep_alive }
                } else {
                    FlushResult::Partial
                }
            }
            WriteResult::WouldBlock => FlushResult::Partial,
            WriteResult::Error(e)   => FlushResult::Error(e),
        }
    } else {
        FlushResult::Error(-1)
    }
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

const HTTP_500: &[u8] = b"\
HTTP/1.1 500 Internal Server Error\r\n\
Content-Type: text/plain\r\n\
Content-Length: 21\r\n\
Connection: close\r\n\
\r\n\
Internal Server Error";

const HTTP_502: &[u8] = b"\
HTTP/1.1 502 Bad Gateway\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Length: 98\r\n\
Connection: close\r\n\
\r\n\
<html><head><title>502 Bad Gateway</title></head><body><h1>502 Bad Gateway</h1></body></html>";

/// Reap all finished child processes (CGI) to prevent zombies.
///
/// Called once per event-loop tick. Returns immediately when no children
/// exist (`ECHILD`) or none have exited yet (`WNOHANG` returns 0).
fn reap_children() {
    loop {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
        eprintln!("[INFO] Reaped child pid={pid} status={status}");
    }
}

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
        ConnectionState::new(fd, 0, 8080, dummy_addr())
    }

    // ---- advance_connection -----------------------------------------------

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
        // With no configs the server falls through to HTTP_500 or 404;
        // either way the response is ready and the phase is WritingResponse.
        assert!(matches!(result, Transition::ResponseReady));
        assert!(matches!(c.phase, ConnectionPhase::WritingResponse { .. }));
    }

    #[test]
    fn oversized_header_block_returns_400_response() {
        let mut c = conn(5);
        c.read_buf.extend(b"GET / HTTP/1.1\r\nX-Pad: ".iter().copied());
        c.read_buf.extend(vec![b'A'; crate::http::request::parser::MAX_HEADER_BYTES + 1]);
        let result = advance_connection(&mut c, &[]);
        assert!(matches!(result, Transition::ResponseReady));
        assert!(c.write_buf.starts_with(b"HTTP/1.1 400"));
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