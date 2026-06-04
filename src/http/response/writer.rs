/// HTTP response serialisation and non-blocking writing.
///
/// `serialize` converts a `Response` into a `Vec<u8>` wire representation.
/// `write_nonblocking` drains as many bytes as the OS will accept and
/// reports whether the write completed or needs to be resumed.
use std::os::unix::io::RawFd;

use crate::http::response::types::{Response, StatusCode};

// ---------------------------------------------------------------------------
// WriteResult
// ---------------------------------------------------------------------------

/// Outcome of a single non-blocking write attempt.
#[derive(Debug)]
pub enum WriteResult {
    /// `n` bytes were written. If `n == buf.len() - offset` the buffer is
    /// exhausted; otherwise the caller must retry from `offset + n`.
    BytesWritten(usize),
    /// `write(2)` returned `EAGAIN` / `EWOULDBLOCK` — the socket send buffer
    /// is full. Caller should re-arm `EPOLLOUT` and retry later.
    WouldBlock,
    /// A fatal write error occurred (`errno` value attached).
    Error(i32),
}

// ---------------------------------------------------------------------------
// serialize
// ---------------------------------------------------------------------------

/// Serialise `response` into a `Vec<u8>` ready for transmission.
///
/// Format (RFC 9112 §4):
/// ```text
/// HTTP/1.1 <status>\r\n
/// <header-name>: <header-value>\r\n
/// ...
/// \r\n
/// <body>
/// ```
///
/// A `Date` header with the current UTC time is injected automatically
/// (RFC 9110 §6.6.1 requires it for responses with a 2xx/3xx/4xx/5xx status).
/// A `Server` header is always appended.
///
/// If `response.status.must_not_have_body()` is true, the body is omitted
/// even if `response.body` is non-empty.
pub fn serialize(response: &Response) -> Vec<u8> {
    // Estimate capacity to avoid repeated reallocs.
    let body_len = if response.status.must_not_have_body() {
        0
    } else {
        response.body.len()
    };
    let mut buf = Vec::with_capacity(256 + body_len);

    // Status line.
    buf.extend_from_slice(b"HTTP/1.1 ");
    buf.extend_from_slice(response.status.to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");

    // User-supplied headers.
    for (name, value) in response.headers.iter() {
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    // Inject standard headers not already set by the builder.
    if response.headers.get("Date").is_none() {
        buf.extend_from_slice(b"Date: ");
        buf.extend_from_slice(rfc7231_date().as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    if response.headers.get("Server").is_none() {
        buf.extend_from_slice(b"Server: localhost/0.1\r\n");
    }

    // Header / body separator.
    buf.extend_from_slice(b"\r\n");

    // Body (omitted for HEAD responses and 1xx / 204 / 304 codes).
    if !response.status.must_not_have_body() {
        buf.extend_from_slice(&response.body);
    }

    buf
}

/// Serialise a response for a `HEAD` request: identical to a `GET` response
/// but with no body bytes. Headers (including `Content-Length`) are kept
/// so the client can learn the resource size.
pub fn serialize_head_response(response: &Response) -> Vec<u8> {
    // Temporarily remove the body for serialisation only.
    let empty_body = Response {
        status:  response.status,
        headers: response.headers.clone(),
        body:    Vec::new(),
    };
    serialize(&empty_body)
}

// ---------------------------------------------------------------------------
// write_nonblocking
// ---------------------------------------------------------------------------

/// Write as many bytes as possible from `buf[offset..]` to `fd`.
///
/// This function does **not** loop — it makes a single `write(2)` call.
/// The dispatcher loop is responsible for calling it again (on the next
/// `EPOLLOUT` event or in a tight flush loop) until `BytesWritten` reports
/// that the full buffer was consumed.
///
/// # Arguments
///
/// * `fd`     — the non-blocking socket file descriptor.
/// * `buf`    — the complete serialised response buffer.
/// * `offset` — how many bytes have already been sent in previous calls.
pub fn write_nonblocking(fd: RawFd, buf: &[u8], offset: usize) -> WriteResult {
    let remaining = &buf[offset..];
    if remaining.is_empty() {
        return WriteResult::BytesWritten(0);
    }

    let n = unsafe {
        libc::write(fd, remaining.as_ptr() as *const libc::c_void, remaining.len())
    };

    if n > 0 {
        return WriteResult::BytesWritten(n as usize);
    }

    if n == 0 {
        // write(2) returning 0 is unusual on a socket but treat as WouldBlock.
        return WriteResult::WouldBlock;
    }

    // n < 0 — check errno.
    let e = unsafe { *libc::__errno_location() };
    if e == libc::EAGAIN || e == libc::EWOULDBLOCK {
        WriteResult::WouldBlock
    } else {
        WriteResult::Error(e)
    }
}

// ---------------------------------------------------------------------------
// Date helper
// ---------------------------------------------------------------------------

/// Format the current UTC time as an RFC 7231 / IMF-fixdate string.
///
/// Example: `Thu, 01 Jan 2026 00:00:00 GMT`
fn rfc7231_date() -> String {
    // We use libc::time / gmtime_r to avoid depending on chrono / time crates.
    let mut t: libc::time_t = 0;
    unsafe { libc::time(&mut t) };

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&t, &mut tm) };

    let days   = ["Sun","Mon","Tue","Wed","Thu","Fri","Sat"];
    let months = ["Jan","Feb","Mar","Apr","May","Jun",
                  "Jul","Aug","Sep","Oct","Nov","Dec"];

    let wday = tm.tm_wday.clamp(0, 6) as usize;
    let mon  = tm.tm_mon .clamp(0, 11) as usize;

    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        days[wday],
        tm.tm_mday,
        months[mon],
        1900 + tm.tm_year,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::response::types::{Response, ResponseHeaders, StatusCode};

    fn simple_ok(body: &[u8]) -> Response {
        let mut resp = Response::new(StatusCode::OK);
        resp.headers.set("Content-Type",   "text/plain");
        resp.headers.set("Content-Length", body.len().to_string());
        resp.body = body.to_vec();
        resp
    }

    // ---- serialize ---------------------------------------------------------

    #[test]
    fn serialize_status_line_and_body() {
        let resp = simple_ok(b"hello");
        let bytes = serialize(&resp);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.ends_with("hello"));
    }

    #[test]
    fn serialize_includes_date_header() {
        let resp = simple_ok(b"");
        let s = String::from_utf8(serialize(&resp)).unwrap();
        assert!(s.contains("Date: "), "missing Date header in: {s}");
    }

    #[test]
    fn serialize_includes_server_header() {
        let resp = simple_ok(b"");
        let s = String::from_utf8(serialize(&resp)).unwrap();
        assert!(s.contains("Server: localhost/0.1"));
    }

    #[test]
    fn serialize_does_not_duplicate_date_if_set() {
        let mut resp = simple_ok(b"");
        resp.headers.set("Date", "Mon, 01 Jan 2024 00:00:00 GMT");
        let s = String::from_utf8(serialize(&resp)).unwrap();
        assert_eq!(s.matches("Date:").count(), 1);
    }

    #[test]
    fn serialize_no_content_omits_body() {
        let mut resp = Response::new(StatusCode::NO_CONTENT);
        resp.headers.set("Content-Length", "0");
        resp.body = b"should not appear".to_vec();
        let s = String::from_utf8(serialize(&resp)).unwrap();
        assert!(!s.contains("should not appear"));
    }

    #[test]
    fn serialize_headers_in_wire_format() {
        let mut resp = Response::new(StatusCode::OK);
        resp.headers.set("Content-Type",   "text/html");
        resp.headers.set("Content-Length", "0");
        let s = String::from_utf8(serialize(&resp)).unwrap();
        assert!(s.contains("Content-Type: text/html\r\n"));
        assert!(s.contains("Content-Length: 0\r\n"));
        // Blank line before body.
        assert!(s.contains("\r\n\r\n"));
    }

    #[test]
    fn serialize_redirect_response() {
        let mut resp = Response::new(StatusCode::MOVED_PERMANENTLY);
        resp.headers.set("Location",       "/new");
        resp.headers.set("Content-Length", "0");
        let s = String::from_utf8(serialize(&resp)).unwrap();
        assert!(s.starts_with("HTTP/1.1 301 Moved Permanently\r\n"));
        assert!(s.contains("Location: /new\r\n"));
    }

    // ---- serialize_head_response -------------------------------------------

    #[test]
    fn head_response_has_headers_but_no_body() {
        let resp = simple_ok(b"hello world");
        let s = String::from_utf8(serialize_head_response(&resp)).unwrap();
        // Headers (including Content-Length) must be present.
        assert!(s.contains("Content-Length: 11\r\n"));
        // But no body bytes after the blank line.
        let body_start = s.find("\r\n\r\n").unwrap() + 4;
        assert!(s[body_start..].is_empty());
    }

    // ---- write_nonblocking -------------------------------------------------

    #[test]
    fn write_nonblocking_returns_bytes_written() {
        let mut fds = [0i32; 2];
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0, fds.as_mut_ptr()) };
        let (rd, wr) = (fds[0], fds[1]);

        let buf = b"HTTP/1.1 200 OK\r\n\r\n";
        let result = write_nonblocking(wr, buf, 0);

        unsafe { libc::close(rd); libc::close(wr); }

        assert!(matches!(result, WriteResult::BytesWritten(n) if n > 0));
    }

    #[test]
    fn write_nonblocking_empty_offset_returns_zero() {
        // When offset == buf.len(), there's nothing to write.
        let buf = b"data";
        let result = write_nonblocking(99, buf, buf.len()); // fd irrelevant
        assert!(matches!(result, WriteResult::BytesWritten(0)));
    }

    // ---- rfc7231_date ------------------------------------------------------

    #[test]
    fn rfc7231_date_format() {
        let d = rfc7231_date();
        // "Www, DD Mmm YYYY HH:MM:SS GMT"
        assert!(d.ends_with(" GMT"), "bad suffix in: {d}");
        assert_eq!(d.len(), 29, "wrong length: {d}");
    }
}