/// Per-connection state machine.
///
/// Each accepted TCP connection is represented by a `ConnectionState` that
/// tracks exactly where the connection is in its request/response lifecycle.
///
/// # Lifecycle
///
/// ```text
///   ReadingHeaders
///       │  (parse_request_head succeeds)
///       ▼
///   ReadingBody ──► (body complete)
///   ReadingChunked ──────────────────────► Processing
///   (no body at all) ───────────────────►      │
///                                    ┌──────────┴──────────┐
///                               WritingResponse        AwaitingCgi
///                                    │                     │
///                                    └──────────┬──────────┘
///                                               ▼
///                                              Done
/// ```
///
/// # Request storage
///
/// The parsed `Request` is stored directly in `ConnectionState` after the
/// header parse completes. This eliminates the "reconstruct_request_with_body"
/// workaround: when the body arrives in subsequent epoll events the method,
/// path, headers, and keep-alive decision are already known.
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::time::Instant;

use crate::http::request::types::Request;
use crate::cgi::CgiProcess;


// ---------------------------------------------------------------------------
// ConnectionPhase
// ---------------------------------------------------------------------------

/// The phase a connection is currently in.
#[derive(Debug)]
pub enum ConnectionPhase {
    /// Accumulating bytes until `\r\n\r\n` is found.
    ReadingHeaders,

    /// Headers parsed; reading a fixed-length body.
    ReadingBody {
        /// Total bytes expected (from `Content-Length`).
        expected:   usize,
        /// Bytes in the body buffer so far.
        bytes_read: usize,
    },

    /// Headers parsed; reading a `Transfer-Encoding: chunked` body.
    ReadingChunked {
        /// Raw chunk-encoded bytes accumulated so far.
        assembled: Vec<u8>,
    },

    /// Full request received; handler is being invoked synchronously.
    /// Transient — never survives a return to `epoll_wait`.
    Processing,

    /// Response serialised into `write_buf`; flushing it non-blocking.
    WritingResponse {
        /// Number of bytes already flushed.
        bytes_written: usize,
    },

    /// CGI child forked; waiting for output on `pipe_fd`.
    AwaitingCgi {
        child_pid: libc::pid_t,
        pipe_fd:   RawFd,
    },

    /// Response fully sent. Dispatcher should close or keep-alive reset.
    Done,
}

// ---------------------------------------------------------------------------
// ConnectionState
// ---------------------------------------------------------------------------

/// All mutable state for one accepted TCP connection.
pub struct ConnectionState {
    /// OS file descriptor for this socket.
    pub fd: RawFd,

    /// Current lifecycle phase.
    pub phase: ConnectionPhase,

    /// Raw bytes received from the client.
    /// `ReadingHeaders`: full received bytes.
    /// `ReadingBody` / `ReadingChunked`: body bytes only.
    pub read_buf: Vec<u8>,

    /// Serialised HTTP response bytes to flush.
    pub write_buf: Vec<u8>,

    /// Parsed request, populated once headers are complete.
    /// Available for the full remainder of the connection lifetime so
    /// body handlers and the dispatcher can access method/path/headers.
    pub request: Option<Request>,

    /// Index into `configs` identifying the `ServerConfig` that owns this
    /// connection (set at accept-time from the listener's server_id).
    pub server_id: usize,

    /// Port on which this connection arrived (needed for virtual-host selection).
    pub local_port: u16,

    /// Timestamp of the last successful I/O on this fd.
    pub last_activity: Instant,

    /// Remote address (for logging and `REMOTE_ADDR` CGI variable).
    pub peer_addr: SocketAddr,

    /// Whether the client negotiated keep-alive.
    /// Set from `Request::is_keep_alive()` as soon as headers are parsed.
    pub keep_alive: bool,

    /// Live CGI child + pipe state while `phase == AwaitingCgi`. `None`
    /// otherwise. Kept separate from `ConnectionPhase::AwaitingCgi` (which
    /// only carries `child_pid`/`pipe_fd` for Debug/timeout purposes) so the
    /// non-blocking I/O helpers in `cgi::io` can operate on it directly.
    pub cgi: Option<CgiProcess>,

    /// Session ID to send back via `Set-Cookie` on the next response, if a
    /// new session was created for the in-flight request. `None` once
    /// attached (or if the request reused an existing session).
    pub pending_session_cookie: Option<crate::session::SessionId>,
}


impl ConnectionState {
    /// Create a new connection in `ReadingHeaders`.
    pub fn new(fd: RawFd, server_id: usize, local_port: u16, peer_addr: SocketAddr) -> Self {
        ConnectionState {
            fd,
            phase:         ConnectionPhase::ReadingHeaders,
            read_buf:      Vec::with_capacity(4096),
            write_buf:     Vec::new(),
            request:       None,
            server_id,
            local_port,
            last_activity: Instant::now(),
            peer_addr,
            keep_alive:    false,
            cgi:                    None,
            pending_session_cookie: None,
        }
    }

    /// Update `last_activity` timestamp. Call after every successful I/O.
    #[inline]
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Returns `true` if more than `timeout` has elapsed since last activity.
    #[inline]
    pub fn is_timed_out(&self, timeout: std::time::Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }

    // ------------------------------------------------------------------ //
    // Phase transitions
    // ------------------------------------------------------------------ //

    /// Store a parsed request and transition to `ReadingBody`.
    ///
    /// `body_so_far` contains bytes already received beyond `\r\n\r\n`.
    pub fn begin_reading_body(&mut self, req: Request, expected: usize, body_so_far: Vec<u8>) {
        self.keep_alive = req.is_keep_alive();
        self.request    = Some(req);
        let bytes_read  = body_so_far.len();
        self.read_buf   = body_so_far;
        self.phase      = ConnectionPhase::ReadingBody { expected, bytes_read };
    }

    /// Store a parsed request and transition to `ReadingChunked`.
    pub fn begin_reading_chunked(&mut self, req: Request, body_so_far: Vec<u8>) {
        self.keep_alive = req.is_keep_alive();
        self.request    = Some(req);
        self.read_buf   = Vec::new();
        self.phase      = ConnectionPhase::ReadingChunked { assembled: body_so_far };
    }

    /// Append newly-received body bytes (used in `ReadingBody`).
    pub fn append_body_bytes(&mut self, new_bytes: &[u8]) {
        if let ConnectionPhase::ReadingBody { ref mut bytes_read, .. } = self.phase {
            self.read_buf.extend_from_slice(new_bytes);
            *bytes_read += new_bytes.len();
        }
    }

    /// Append newly-received bytes to the chunked assembler.
    pub fn append_chunked_bytes(&mut self, new_bytes: &[u8]) {
        if let ConnectionPhase::ReadingChunked { ref mut assembled } = self.phase {
            assembled.extend_from_slice(new_bytes);
        }
    }

    /// Attach the decoded body to the stored request and transition to `Processing`.
    pub fn complete_body(&mut self, body: Vec<u8>) {
        if let Some(ref mut req) = self.request {
            req.body = body;
        }
        self.read_buf.clear();
        self.phase = ConnectionPhase::Processing;
    }

    /// Queue a serialised response and transition to `WritingResponse`.
    pub fn set_response(&mut self, response_bytes: Vec<u8>) {
        self.write_buf = response_bytes;
        self.phase     = ConnectionPhase::WritingResponse { bytes_written: 0 };
    }

    /// Called when the response is fully flushed.
    ///
    /// Returns `true` if the connection is kept alive (caller should re-arm
    /// `EPOLLIN`). Returns `false` if the connection should be closed.
    pub fn finish(&mut self) -> bool {
        if self.keep_alive {
            self.read_buf.clear();
            self.write_buf.clear();
            self.request = None;
            self.cgi                    = None;
            self.pending_session_cookie = None;
            self.phase   = ConnectionPhase::ReadingHeaders;
            self.touch();
            true
        } else {
            self.phase = ConnectionPhase::Done;
            false
        }
    }

    /// `true` when the connection should be removed from the registry.
    #[inline]
    pub fn is_done(&self) -> bool {
        matches!(self.phase, ConnectionPhase::Done)
    }

    /// `true` while we are still accumulating request data.
    pub fn is_reading(&self) -> bool {
        matches!(
            self.phase,
            ConnectionPhase::ReadingHeaders
                | ConnectionPhase::ReadingBody { .. }
                | ConnectionPhase::ReadingChunked { .. }
        )
    }

    /// `true` while we are writing the response.
    pub fn is_writing(&self) -> bool {
        matches!(self.phase, ConnectionPhase::WritingResponse { .. })
    }
}

impl std::fmt::Debug for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionState")
            .field("fd",            &self.fd)
            .field("phase",         &self.phase)
            .field("read_buf_len",  &self.read_buf.len())
            .field("write_buf_len", &self.write_buf.len())
            .field("has_request",   &self.request.is_some())
            .field("server_id",     &self.server_id)
            .field("local_port",    &self.local_port)
            .field("peer_addr",     &self.peer_addr)
            .field("keep_alive",    &self.keep_alive)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::request::types::{HeaderMap, Method, Version};
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    fn dummy_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345)
    }

    fn make_conn() -> ConnectionState {
        ConnectionState::new(5, 0, 8080, dummy_addr())
    }

    fn minimal_request() -> Request {
        Request {
            method:  Method::Get,
            path:    "/".into(),
            query:   String::new(),
            version: Version::Http11,
            headers: HeaderMap::new(),
            body:    Vec::new(),
        }
    }

    #[test]
    fn new_starts_in_reading_headers() {
        let c = make_conn();
        assert!(matches!(c.phase, ConnectionPhase::ReadingHeaders));
        assert!(c.read_buf.is_empty());
        assert!(c.write_buf.is_empty());
        assert!(c.request.is_none());
        assert!(!c.keep_alive);
        assert_eq!(c.local_port, 8080);
    }

    #[test]
    fn begin_reading_body_stores_request_and_phase() {
        let mut c = make_conn();
        c.begin_reading_body(minimal_request(), 10, b"hello".to_vec());
        assert!(matches!(c.phase, ConnectionPhase::ReadingBody { expected: 10, bytes_read: 5 }));
        assert!(c.request.is_some());
        assert_eq!(c.read_buf, b"hello");
    }

    #[test]
    fn begin_reading_chunked_stores_request() {
        let mut c = make_conn();
        c.begin_reading_chunked(minimal_request(), b"5\r\n".to_vec());
        assert!(matches!(c.phase, ConnectionPhase::ReadingChunked { .. }));
        assert!(c.request.is_some());
    }

    #[test]
    fn append_body_bytes_extends_read_buf() {
        let mut c = make_conn();
        c.begin_reading_body(minimal_request(), 10, b"hel".to_vec());
        c.append_body_bytes(b"lo");
        assert_eq!(c.read_buf, b"hello");
        assert!(matches!(c.phase, ConnectionPhase::ReadingBody { bytes_read: 5, .. }));
    }

    #[test]
    fn complete_body_attaches_body_and_sets_processing() {
        let mut c = make_conn();
        c.request = Some(minimal_request());
        c.complete_body(b"world".to_vec());
        assert!(matches!(c.phase, ConnectionPhase::Processing));
        assert_eq!(c.request.as_ref().unwrap().body, b"world");
        assert!(c.read_buf.is_empty());
    }

    #[test]
    fn set_response_transitions_to_writing() {
        let mut c = make_conn();
        c.set_response(b"HTTP/1.1 200 OK\r\n\r\n".to_vec());
        assert!(matches!(c.phase, ConnectionPhase::WritingResponse { bytes_written: 0 }));
        assert!(!c.write_buf.is_empty());
    }

    #[test]
    fn finish_without_keep_alive_becomes_done() {
        let mut c = make_conn();
        c.keep_alive = false;
        assert!(!c.finish());
        assert!(c.is_done());
    }

    #[test]
    fn finish_with_keep_alive_resets_everything() {
        let mut c    = make_conn();
        c.keep_alive = true;
        c.request    = Some(minimal_request());
        c.read_buf   = b"leftover".to_vec();
        c.write_buf  = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        assert!(c.finish());
        assert!(!c.is_done());
        assert!(c.request.is_none());
        assert!(c.read_buf.is_empty());
        assert!(c.write_buf.is_empty());
        assert!(matches!(c.phase, ConnectionPhase::ReadingHeaders));
    }

    #[test]
    fn is_reading_correct_phases() {
        let mut c = make_conn();
        assert!(c.is_reading()); // ReadingHeaders
        c.phase = ConnectionPhase::ReadingBody { expected: 5, bytes_read: 0 };
        assert!(c.is_reading());
        c.phase = ConnectionPhase::ReadingChunked { assembled: vec![] };
        assert!(c.is_reading());
        c.phase = ConnectionPhase::WritingResponse { bytes_written: 0 };
        assert!(!c.is_reading());
        c.phase = ConnectionPhase::Done;
        assert!(!c.is_reading());
    }

    #[test]
    fn is_writing_only_in_writing_phase() {
        let mut c = make_conn();
        assert!(!c.is_writing());
        c.phase = ConnectionPhase::WritingResponse { bytes_written: 0 };
        assert!(c.is_writing());
        c.phase = ConnectionPhase::Done;
        assert!(!c.is_writing());
    }

    #[test]
    fn timeout_zero_duration_always_triggered() {
        let c = make_conn();
        assert!(c.is_timed_out(Duration::from_secs(0)));
    }

    #[test]
    fn touch_resets_timeout_clock() {
        let mut c = make_conn();
        std::thread::sleep(Duration::from_millis(2));
        assert!(c.is_timed_out(Duration::from_millis(1)));
        c.touch();
        assert!(!c.is_timed_out(Duration::from_secs(10)));
    }

    #[test]
    fn debug_does_not_panic() {
        let _ = format!("{:?}", make_conn());
    }
}