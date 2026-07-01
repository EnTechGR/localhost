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

/// Default time allowed to receive complete request headers.
pub const TIMEOUT_HEADERS_CONST: Duration = Duration::from_secs(30);

/// Time allowed to receive complete request headers.
///
/// Can be overridden via `TIMEOUT_HEADERS_SECS` environment variable (for tests).
pub fn get_timeout_headers() -> Duration {
    std::env::var("TIMEOUT_HEADERS_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(TIMEOUT_HEADERS_CONST)
}

/// Deprecated: use `get_timeout_headers()` instead. Public for backward compat.
#[allow(dead_code)]
pub const TIMEOUT_HEADERS: Duration = TIMEOUT_HEADERS_CONST;

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
            Some((get_timeout_headers(), TimeoutClass::RequestTimeout))
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

    #[test]
    fn reading_headers_is_request_timeout() {
        let (dur, class) = timeout_for_phase(&ConnectionPhase::ReadingHeaders).unwrap();
        assert_eq!(class, TimeoutClass::RequestTimeout);
        // Use the constant since get_timeout_headers() may be env-var overridden.
        assert_eq!(dur,   TIMEOUT_HEADERS_CONST);
    }

    #[test]
    fn reading_body_is_request_timeout() {
        let (dur, class) = timeout_for_phase(&ConnectionPhase::ReadingBody {
            expected: 100,
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
        let (dur, class) = timeout_for_phase(&ConnectionPhase::AwaitingCgi).unwrap();
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
            ConnectionPhase::ReadingBody    { expected: 10 },
            ConnectionPhase::ReadingChunked { assembled: vec![] },
            ConnectionPhase::Processing,
            ConnectionPhase::WritingResponse { bytes_written: 0 },
            ConnectionPhase::AwaitingCgi,
        ];
        for phase in &phases {
            let (dur, _) = timeout_for_phase(phase)
                .unwrap_or_else(|| panic!("no timeout for {phase:?}"));
            assert!(dur.as_millis() > 0, "zero timeout for {phase:?}");
        }
    }

    // ------------------------------------------------------------------
    // Per-phase response content verification
    // ------------------------------------------------------------------

    /// Every phase that can be timed out must map to a canned response with
    // the correct HTTP status line and body.
    #[test]
    fn phases_timed_out_with_408_send_correct_response() {
        for phase in [
            ConnectionPhase::ReadingHeaders,
            ConnectionPhase::ReadingBody    { expected: 10 },
            ConnectionPhase::ReadingChunked { assembled: vec![] },
        ] {
            let (dur, class) = timeout_for_phase(&phase)
                .unwrap_or_else(|| panic!("no timeout for {phase:?}"));
            assert!(dur.as_secs() > 0, "positive timeout for {phase:?}");

            let resp = canned_response(class);
            let s      = std::str::from_utf8(resp).expect("408 must be valid UTF-8");
            assert!(s.starts_with("HTTP/1.1 408"), "phase {:?} should yield 408 but got: {}", phase, s.lines().next().unwrap_or("(empty)"));
        }
    }

    #[test]
    fn phases_timed_out_with_504_send_correct_response() {
        for phase in [
            ConnectionPhase::Processing,
            ConnectionPhase::WritingResponse  { bytes_written: 0 },
            ConnectionPhase::AwaitingCgi,
        ] {
            let (dur, class) = timeout_for_phase(&phase)
                .unwrap_or_else(|| panic!("no timeout for {phase:?}"));
            assert!(dur.as_secs() > 0, "positive timeout for {phase:?}");

            let resp = canned_response(class);
            let s      = std::str::from_utf8(resp).expect("504 must be valid UTF-8");
            assert!(s.starts_with("HTTP/1.1 504"), "phase {:?} should yield 504 but got: {}", phase, s.lines().next().unwrap_or("(empty)"));
        }
    }

    #[test]
    fn done_phase_never_times_out() {
        assert!(timeout_for_phase(&ConnectionPhase::Done).is_none(), "Done must never time out");

        // Even with a zero timeout, Done is not timed out (no entry in map).
        let resp = canned_response(TimeoutClass::GatewayTimeout);
        assert!(resp.starts_with(b"HTTP/1.1 504"), "canned 504 must start correctly");
    }

    #[test]
    fn all_timeout_responses_have_matching_content_length() {
        // 408: verify body length matches the Content-Length header value.
        let raw = RESPONSE_408;
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let cl_header = std::str::from_utf8(&raw[..sep]).unwrap()
            .lines().find(|l| l.to_lowercase().starts_with("content-length:"))
            .expect("408 must have Content-Length header");
        let cl: usize = cl_header.split(':').nth(1).unwrap().trim().parse().unwrap();
        assert_eq!(cl, raw.len() - sep - 4, "408 Content-Length must match actual body length");

        // 504: same check.
        let raw = RESPONSE_504;
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let cl_header = std::str::from_utf8(&raw[..sep]).unwrap()
            .lines().find(|l| l.to_lowercase().starts_with("content-length:"))
            .expect("504 must have Content-Length header");
        let cl: usize = cl_header.split(':').nth(1).unwrap().trim().parse().unwrap();
        assert_eq!(cl, raw.len() - sep - 4, "504 Content-Length must match actual body length");
    }

    #[test]
    fn all_timeout_responses_close_connection() {
        for class in [TimeoutClass::RequestTimeout, TimeoutClass::GatewayTimeout] {
            let resp = canned_response(class);
            let s = std::str::from_utf8(resp).expect("response must be valid UTF-8");
            assert!(s.contains("\r\nConnection: close\r\n"),
                    "timeout response {:?} must include Connection: close", class);
        }
    }

    #[test]
    fn timeout_constants_have_reasonable_durations() {
        // Reading headers should timeout fastest (slowloris defense).
        assert!(TIMEOUT_HEADERS.as_secs() <= 45, "headers timeout should be ≤45s");

        // Body / write timeouts can be longer for uploads.
        assert!(TIMEOUT_BODY.as_secs() >= 30, "body timeout should be ≥30s");
        assert!(TIMEOUT_WRITE.as_secs() >= 30, "write timeout should be ≥30s");

        // Processing is a safety net; shouldn't be excessive.
        assert!(TIMEOUT_PROCESSING.as_secs() <= 10, "processing timeout should be ≤10s");

        // CGI scripts should not run indefinitely.
        assert!(TIMEOUT_CGI.as_secs() >= 15 && TIMEOUT_CGI.as_secs() <= 60,
                "CGI timeout should be between 15s and 60s");
    }
}