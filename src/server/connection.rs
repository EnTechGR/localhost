/// Per-connection state machine.
///
/// Each accepted TCP connection is represented by a `ConnectionState` that
/// tracks exactly where the connection is in its request/response lifecycle.
/// The dispatcher drives transitions by calling `advance()` after each
/// non-blocking I/O operation.
///
/// # Lifecycle
///
/// ```text
///   ReadingHeaders
///       │  (headers complete)
///       ▼
///   ReadingBody  ──(body complete or no body)──►  Processing
///       │                                              │
///   ReadingChunked                         ┌──────────┴──────────┐
///                                          │                     │
///                                     WritingResponse       AwaitingCgi
///                                          │                     │
///                                          └──────────┬──────────┘
///                                                     ▼
///                                                    Done
/// ```
///
/// `Done` signals to the dispatcher that the connection should be closed
/// (or kept alive if `Connection: keep-alive` was negotiated — in that case
/// the state resets to `ReadingHeaders` and the read buffer is cleared).
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::time::Instant;

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
        /// Bytes already in `read_buf` past the header boundary.
        bytes_read: usize,
    },

    /// Headers parsed; reading a `Transfer-Encoding: chunked` body.
    ReadingChunked {
        /// Partial chunk data assembled so far.
        assembled: Vec<u8>,
    },

    /// Full request received; handler is being invoked synchronously.
    /// This phase is transient — it never survives a return to `epoll_wait`.
    Processing,

    /// Response serialised into `write_buf`; writing it out non-blocking.
    WritingResponse {
        /// Bytes already flushed to the socket.
        bytes_written: usize,
    },

    /// CGI process forked; waiting for it to produce output on `pipe_fd`.
    AwaitingCgi {
        /// PID of the forked CGI child.
        child_pid: libc::pid_t,
        /// Read-end of the pipe connected to the CGI's stdout.
        pipe_fd:   RawFd,
    },

    /// Response fully sent. Dispatcher should close or reset the connection.
    Done,
}

// ---------------------------------------------------------------------------
// ConnectionState
// ---------------------------------------------------------------------------

/// All mutable state associated with one accepted TCP connection.
pub struct ConnectionState {
    /// The OS file descriptor for this connection.
    pub fd: RawFd,

    /// Current lifecycle phase.
    pub phase: ConnectionPhase,

    /// Raw bytes received from the client.
    /// Grows as `EPOLLIN` fires; consumed by the request parser.
    pub read_buf: Vec<u8>,

    /// Serialised HTTP response bytes waiting to be sent.
    /// Populated in `Processing`; drained in `WritingResponse`.
    pub write_buf: Vec<u8>,

    /// Index into `configs` identifying which `ServerConfig` accepted this
    /// connection (set at accept-time from the listener's server_id).
    pub server_id: usize,

    /// When data was last successfully read or written on this fd.
    /// Used by the timeout checker to evict stalled connections.
    pub last_activity: Instant,

    /// Remote address, for logging and `REMOTE_ADDR` CGI variable.
    pub peer_addr: SocketAddr,

    /// Whether the client sent `Connection: keep-alive`.
    /// When `true` and the response is complete, reset to `ReadingHeaders`
    /// instead of moving to `Done`.
    pub keep_alive: bool,
}

impl ConnectionState {
    /// Create a new connection in the initial `ReadingHeaders` phase.
    pub fn new(fd: RawFd, server_id: usize, peer_addr: SocketAddr) -> Self {
        ConnectionState {
            fd,
            phase:         ConnectionPhase::ReadingHeaders,
            read_buf:      Vec::with_capacity(4096),
            write_buf:     Vec::new(),
            server_id,
            last_activity: Instant::now(),
            peer_addr,
            keep_alive:    false,
        }
    }

    /// Update `last_activity` timestamp. Called after every successful I/O.
    #[inline]
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Returns `true` if `elapsed` has passed since the last I/O activity.
    #[inline]
    pub fn is_timed_out(&self, timeout: std::time::Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }

    /// Transition to `WritingResponse`, copying the serialised response bytes
    /// into `write_buf` and resetting the write cursor to 0.
    pub fn set_response(&mut self, response_bytes: Vec<u8>) {
        self.write_buf = response_bytes;
        self.phase = ConnectionPhase::WritingResponse { bytes_written: 0 };
    }

    /// Mark the connection as done. If `keep_alive` is set, instead resets
    /// all buffers and returns the connection to `ReadingHeaders`.
    ///
    /// Returns `true` if the connection should be kept open.
    pub fn finish(&mut self) -> bool {
        if self.keep_alive {
            self.read_buf.clear();
            self.write_buf.clear();
            self.phase = ConnectionPhase::ReadingHeaders;
            self.touch();
            true
        } else {
            self.phase = ConnectionPhase::Done;
            false
        }
    }

    /// Returns `true` if the connection is in a terminal state and should
    /// be removed from the registry and closed.
    pub fn is_done(&self) -> bool {
        matches!(self.phase, ConnectionPhase::Done)
    }
}

impl std::fmt::Debug for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionState")
            .field("fd",          &self.fd)
            .field("phase",       &self.phase)
            .field("read_buf",    &self.read_buf.len())
            .field("write_buf",   &self.write_buf.len())
            .field("server_id",   &self.server_id)
            .field("peer_addr",   &self.peer_addr)
            .field("keep_alive",  &self.keep_alive)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    fn dummy_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345)
    }

    #[test]
    fn new_connection_starts_reading_headers() {
        let conn = ConnectionState::new(5, 0, dummy_addr());
        assert!(matches!(conn.phase, ConnectionPhase::ReadingHeaders));
        assert!(conn.read_buf.is_empty());
        assert!(conn.write_buf.is_empty());
        assert!(!conn.keep_alive);
    }

    #[test]
    fn set_response_transitions_to_writing() {
        let mut conn = ConnectionState::new(5, 0, dummy_addr());
        conn.set_response(b"HTTP/1.1 200 OK\r\n\r\n".to_vec());
        assert!(matches!(
            conn.phase,
            ConnectionPhase::WritingResponse { bytes_written: 0 }
        ));
        assert!(!conn.write_buf.is_empty());
    }

    #[test]
    fn finish_without_keep_alive_sets_done() {
        let mut conn = ConnectionState::new(5, 0, dummy_addr());
        conn.keep_alive = false;
        let keep_open = conn.finish();
        assert!(!keep_open);
        assert!(conn.is_done());
    }

    #[test]
    fn finish_with_keep_alive_resets_to_reading_headers() {
        let mut conn = ConnectionState::new(5, 0, dummy_addr());
        conn.keep_alive = true;
        conn.read_buf  = b"leftover data".to_vec();
        conn.write_buf = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        let keep_open = conn.finish();
        assert!(keep_open);
        assert!(!conn.is_done());
        assert!(conn.read_buf.is_empty());
        assert!(conn.write_buf.is_empty());
        assert!(matches!(conn.phase, ConnectionPhase::ReadingHeaders));
    }

    #[test]
    fn timeout_detection() {
        let conn = ConnectionState::new(5, 0, dummy_addr());
        // Brand-new connection should not be timed out with a 10s window.
        assert!(!conn.is_timed_out(Duration::from_secs(10)));
        // But it should be timed out with a 0-duration window.
        assert!(conn.is_timed_out(Duration::from_secs(0)));
    }

    #[test]
    fn touch_resets_timeout_clock() {
        let mut conn = ConnectionState::new(5, 0, dummy_addr());
        // Sleep a tiny bit so elapsed() is non-zero.
        std::thread::sleep(Duration::from_millis(2));
        assert!(conn.is_timed_out(Duration::from_millis(1)));
        conn.touch();
        // After touch, a 10s window should pass.
        assert!(!conn.is_timed_out(Duration::from_secs(10)));
    }

    #[test]
    fn debug_impl_does_not_panic() {
        let conn = ConnectionState::new(5, 0, dummy_addr());
        let _ = format!("{conn:?}");
    }
}