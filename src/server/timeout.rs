/// Connection timeout management.
///
/// This module owns the timeout constants and provides the logic for mapping
/// a connection's current phase to the correct timeout window. The dispatcher
/// calls `timeout_for_phase` when deciding whether to evict a connection, and
/// `TimeoutClass` when choosing the error response to send.
///
/// # Design
///
/// Timeouts are phase-specific because the expected latency differs:
///
/// | Phase              | Timeout | Rationale |
/// |--------------------|---------|-----------|
/// | ReadingHeaders     | 30 s    | Slow-loris attack defence |
/// | ReadingBody        | 60 s    | Large uploads need time |
/// | ReadingChunked     | 60 s    | Same as body |
/// | WritingResponse    | 60 s    | Client may have a slow link |
/// | AwaitingCgi        | 30 s    | CGI scripts should be fast |
/// | Processing         |  5 s    | Synchronous; should never linger |
/// | Done               |  0 s    | Should be removed immediately |
///
/// These values are intentionally conservative. The spec says the server must
/// "never crash" and that "all requests timeout if they are taking too long".
use std::time::Duration;

use crate::server::connection::ConnectionPhase;

// ---------------------------------------------------------------------------
// Timeout constants
// ---------------------------------------------------------------------------

/// Time allowed to receive complete request headers.
pub const TIMEOUT_HEADERS: Duration = Duration::from_secs(30);

/// Time allowed to receive the complete request body (fixed-length or chunked).
pub const TIMEOUT_BODY: Duration = Duration::from_secs(60);

/// Time allowed to flush the entire response to the client.
pub const TIMEOUT_WRITE: Duration = Duration::from_secs(60);

/// Time allowed for a CGI child process to produce its full output.
pub const TIMEOUT_CGI: Duration = Duration::from_secs(30);

/// Synchronous processing should finish in microseconds; cap it generously.
pub const TIMEOUT_PROCESSING: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// TimeoutClass
// ---------------------------------------------------------------------------

/// Which kind of timeout fired — determines the error response sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutClass {
    /// Request was not completed in time → send 408 Request Timeout.
    RequestTimeout,
    /// CGI or write stalled → send 504 Gateway Timeout.
    GatewayTimeout,
}

// ---------------------------------------------------------------------------
// Phase → timeout mapping
// ---------------------------------------------------------------------------

/// Return the timeout duration and class for a given connection phase.
///
/// `None` means the phase should never be timed out (e.g. `Done`).
pub fn timeout_for_phase(phase: &ConnectionPhase) -> Option<(Duration, TimeoutClass)> {
    match phase {
        ConnectionPhase::ReadingHeaders => {
            Some((TIMEOUT_HEADERS, TimeoutClass::RequestTimeout))
        }
        ConnectionPhase::ReadingBody { .. }
        | ConnectionPhase::ReadingChunked { .. } => {
            Some((TIMEOUT_BODY, TimeoutClass::RequestTimeout))
        }
        ConnectionPhase::WritingResponse { .. } => {
            Some((TIMEOUT_WRITE, TimeoutClass::GatewayTimeout))
        }
        ConnectionPhase::AwaitingCgi { .. } => {
            Some((TIMEOUT_CGI, TimeoutClass::GatewayTimeout))
        }
        ConnectionPhase::Processing => {
            // Synchronous processing should never take this long, but guard it.
            Some((TIMEOUT_PROCESSING, TimeoutClass::GatewayTimeout))
        }
        ConnectionPhase::Done => None,
    }
}

// ---------------------------------------------------------------------------
// Canned timeout responses
// ---------------------------------------------------------------------------

/// Pre-serialised `408 Request Timeout` response.
pub const RESPONSE_408: &[u8] = b"\
HTTP/1.1 408 Request Timeout\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Length: 101\r\n\
Connection: close\r\n\
\r\n\
<html><head><title>408 Request Timeout</title></head><body><h1>408 Request Timeout</h1></body></html>";

/// Pre-serialised `504 Gateway Timeout` response.
pub const RESPONSE_504: &[u8] = b"\
HTTP/1.1 504 Gateway Timeout\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Length: 101\r\n\
Connection: close\r\n\
\r\n\
<html><head><title>504 Gateway Timeout</title></head><body><h1>504 Gateway Timeout</h1></body></html>";

/// Return the canned response for a given `TimeoutClass`.
pub fn canned_response(class: TimeoutClass) -> &'static [u8] {
    match class {
        TimeoutClass::RequestTimeout => RESPONSE_408,
        TimeoutClass::GatewayTimeout => RESPONSE_504,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::connection::ConnectionPhase;
    use std::os::unix::io::RawFd;

    #[test]
    fn reading_headers_is_request_timeout() {
        let (dur, class) = timeout_for_phase(&ConnectionPhase::ReadingHeaders).unwrap();
        assert_eq!(class, TimeoutClass::RequestTimeout);
        assert_eq!(dur,   TIMEOUT_HEADERS);
    }

    #[test]
    fn reading_body_is_request_timeout() {
        let (dur, class) = timeout_for_phase(&ConnectionPhase::ReadingBody {
            expected: 100, bytes_read: 0,
        }).unwrap();
        assert_eq!(class, TimeoutClass::RequestTimeout);
        assert_eq!(dur,   TIMEOUT_BODY);
    }

    #[test]
    fn reading_chunked_is_request_timeout() {
        let (_, class) = timeout_for_phase(&ConnectionPhase::ReadingChunked {
            assembled: vec![],
        }).unwrap();
        assert_eq!(class, TimeoutClass::RequestTimeout);
    }

    #[test]
    fn writing_response_is_gateway_timeout() {
        let (dur, class) = timeout_for_phase(&ConnectionPhase::WritingResponse {
            bytes_written: 0,
        }).unwrap();
        assert_eq!(class, TimeoutClass::GatewayTimeout);
        assert_eq!(dur,   TIMEOUT_WRITE);
    }

    #[test]
    fn awaiting_cgi_is_gateway_timeout() {
        let (dur, class) = timeout_for_phase(&ConnectionPhase::AwaitingCgi {
            child_pid: 0,
            pipe_fd:   0 as RawFd,
        }).unwrap();
        assert_eq!(class, TimeoutClass::GatewayTimeout);
        assert_eq!(dur,   TIMEOUT_CGI);
    }

    #[test]
    fn processing_has_short_timeout() {
        let (dur, _) = timeout_for_phase(&ConnectionPhase::Processing).unwrap();
        assert_eq!(dur, TIMEOUT_PROCESSING);
    }

    #[test]
    fn done_returns_none() {
        assert!(timeout_for_phase(&ConnectionPhase::Done).is_none());
    }

    #[test]
    fn canned_response_408_is_valid_http() {
        let s = std::str::from_utf8(RESPONSE_408).unwrap();
        assert!(s.starts_with("HTTP/1.1 408"));
        assert!(s.contains("Content-Length:"));
        assert!(s.contains("\r\n\r\n"));
    }

    #[test]
    fn canned_response_504_is_valid_http() {
        let s = std::str::from_utf8(RESPONSE_504).unwrap();
        assert!(s.starts_with("HTTP/1.1 504"));
    }

    #[test]
    fn canned_response_content_length_matches_body() {
        // Verify the Content-Length header in 408 is accurate.
        let raw    = RESPONSE_408;
        let sep    = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let header = std::str::from_utf8(&raw[..sep]).unwrap();
        let body   = &raw[sep + 4..];
        let cl_line = header.lines()
            .find(|l| l.to_lowercase().starts_with("content-length:"))
            .unwrap();
        let cl: usize = cl_line.split(':').nth(1).unwrap().trim().parse().unwrap();
        assert_eq!(cl, body.len());

        // Same check for 504.
        let raw    = RESPONSE_504;
        let sep    = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let header = std::str::from_utf8(&raw[..sep]).unwrap();
        let body   = &raw[sep + 4..];
        let cl_line = header.lines()
            .find(|l| l.to_lowercase().starts_with("content-length:"))
            .unwrap();
        let cl: usize = cl_line.split(':').nth(1).unwrap().trim().parse().unwrap();
        assert_eq!(cl, body.len());
    }

    #[test]
    fn all_non_done_phases_have_positive_timeouts() {
        let phases = vec![
            ConnectionPhase::ReadingHeaders,
            ConnectionPhase::ReadingBody    { expected: 10, bytes_read: 0 },
            ConnectionPhase::ReadingChunked { assembled: vec![] },
            ConnectionPhase::Processing,
            ConnectionPhase::WritingResponse { bytes_written: 0 },
            ConnectionPhase::AwaitingCgi    { child_pid: 0, pipe_fd: 0 },
        ];
        for phase in &phases {
            let (dur, _) = timeout_for_phase(phase)
                .unwrap_or_else(|| panic!("no timeout for {phase:?}"));
            assert!(dur.as_millis() > 0, "zero timeout for {phase:?}");
        }
    }
}