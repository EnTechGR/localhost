/// HTTP body reading: fixed-length and chunked transfer encoding.
///
/// Both functions operate on a byte slice that is the *body portion* of the
/// read buffer — i.e. the bytes **after** the `\r\n\r\n` header terminator.
/// The caller must track the split point (returned as `consumed` by the
/// header parser).
///
/// # Chunked encoding (RFC 9112 §7.1)
///
/// Each chunk has the form:
/// ```text
/// <hex-size>[extensions]\r\n
/// <chunk-data>\r\n
/// ```
/// The final chunk is size `0`, optionally followed by trailers (which we
/// discard) and `\r\n`.
use crate::http::request::types::ParseError;

// ---------------------------------------------------------------------------
// Unchunked body
// ---------------------------------------------------------------------------

/// Result of attempting to read a fixed-length body.
#[derive(Debug)]
pub enum BodyResult {
    /// All `content_length` bytes are present in `buf`.
    /// Returns the body bytes (a slice of `buf`).
    Complete(Vec<u8>),
    /// `buf` has fewer bytes than `content_length`. Accumulate more.
    NeedsMore { have: usize, need: usize },
}

/// Attempt to read a fixed-length body from the body portion of the buffer.
///
/// `buf`            — bytes after the header terminator.
/// `content_length` — value of the `Content-Length` header.
pub fn read_body_unchunked(buf: &[u8], content_length: usize) -> BodyResult {
    if buf.len() >= content_length {
        BodyResult::Complete(buf[..content_length].to_vec())
    } else {
        BodyResult::NeedsMore {
            have: buf.len(),
            need: content_length,
        }
    }
}

// ---------------------------------------------------------------------------
// Chunked body
// ---------------------------------------------------------------------------

/// Result of attempting to decode a chunked body.
#[derive(Debug)]
pub enum ChunkedResult {
    /// All chunks decoded; returns the fully assembled body.
    /// The associated `usize` is how many bytes of `buf` were consumed
    /// (including the final `0\r\n\r\n` terminator).
    Complete(Vec<u8>, usize),
    /// More data needed before decoding can complete.
    NeedsMore,
    /// The chunk encoding is malformed.
    Error(ParseError),
}

/// Decode a `Transfer-Encoding: chunked` body from `buf`.
///
/// This is a full, non-allocating pass over `buf`: if any chunk boundary is
/// incomplete, the function returns `NeedsMore` without retaining state. The
/// caller retains the entire body portion in the read buffer and retries on
/// the next EPOLLIN event.
///
/// Chunk extensions (`;name=value`) are accepted syntactically but discarded.
/// Trailing headers are discarded.
pub fn read_body_chunked(buf: &[u8]) -> ChunkedResult {
    let mut assembled = Vec::new();
    let mut pos       = 0;

    loop {
        // ---- Read chunk size line ----------------------------------------
        let line_end = match memchr_crlf(&buf[pos..]) {
            Some(off) => pos + off,
            None      => return ChunkedResult::NeedsMore,
        };

        let size_line = match std::str::from_utf8(&buf[pos..line_end]) {
            Ok(s)  => s,
            Err(_) => return ChunkedResult::Error(
                ParseError::BadChunkSize("non-UTF-8 chunk size line".into())
            ),
        };

        // Strip optional chunk extensions (everything after ';').
        let size_str = size_line.split(';').next().unwrap_or("").trim();

        let chunk_size = match usize::from_str_radix(size_str, 16) {
            Ok(n)  => n,
            Err(_) => return ChunkedResult::Error(
                ParseError::BadChunkSize(format!("invalid hex size '{size_str}'"))
            ),
        };

        pos = line_end + 2; // skip \r\n after size

        // ---- Terminal chunk ----------------------------------------------
        if chunk_size == 0 {
            // Skip any trailing headers until the final \r\n\r\n or bare \r\n.
            let rest = &buf[pos..];
            let trailer_end = find_trailer_end(rest);
            match trailer_end {
                Some(off) => {
                    let consumed = pos + off;
                    return ChunkedResult::Complete(assembled, consumed);
                }
                None => return ChunkedResult::NeedsMore,
            }
        }

        // ---- Chunk data --------------------------------------------------
        let data_end = pos + chunk_size;
        if buf.len() < data_end + 2 {
            // Not enough bytes for chunk data + trailing \r\n.
            return ChunkedResult::NeedsMore;
        }

        // Verify trailing \r\n after chunk data.
        if &buf[data_end..data_end + 2] != b"\r\n" {
            return ChunkedResult::Error(ParseError::BadChunkSize(
                "missing CRLF after chunk data".into()
            ));
        }

        assembled.extend_from_slice(&buf[pos..data_end]);
        pos = data_end + 2; // advance past chunk data + \r\n
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the position of the first `\r\n` in `buf`.
/// Returns the index of the `\r` byte.
fn memchr_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// After the final `0\r\n` of a chunked body, find the end of the optional
/// trailers section. Returns the number of bytes consumed, or `None` if
/// more data is needed.
///
/// The trailer section ends with `\r\n\r\n` (trailers present) or just `\r\n`
/// (no trailers, i.e. the `0\r\n` was immediately followed by a blank line).
fn find_trailer_end(rest: &[u8]) -> Option<usize> {
    // No trailers: just "\r\n" immediately.
    if rest.len() >= 2 && &rest[..2] == b"\r\n" {
        return Some(2);
    }
    // Trailers: scan for "\r\n\r\n".
    rest.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- unchunked ---------------------------------------------------------

    #[test]
    fn unchunked_complete_when_enough_bytes() {
        let body = b"Hello, world!";
        match read_body_unchunked(body, 5) {
            BodyResult::Complete(v) => assert_eq!(v, b"Hello"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unchunked_exact_length() {
        let body = b"Hello";
        match read_body_unchunked(body, 5) {
            BodyResult::Complete(v) => assert_eq!(v, b"Hello"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unchunked_needs_more_when_short() {
        let body = b"Hi";
        match read_body_unchunked(body, 10) {
            BodyResult::NeedsMore { have: 2, need: 10 } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unchunked_empty_body_zero_length() {
        match read_body_unchunked(b"", 0) {
            BodyResult::Complete(v) => assert!(v.is_empty()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- chunked -----------------------------------------------------------

    #[test]
    fn chunked_single_chunk() {
        // "5\r\nHello\r\n0\r\n\r\n"
        let data = b"5\r\nHello\r\n0\r\n\r\n";
        match read_body_chunked(data) {
            ChunkedResult::Complete(body, consumed) => {
                assert_eq!(body, b"Hello");
                assert_eq!(consumed, data.len());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn chunked_multiple_chunks() {
        // "5\r\nHello\r\n6\r\n World\r\n0\r\n\r\n"
        let data = b"5\r\nHello\r\n6\r\n World\r\n0\r\n\r\n";
        match read_body_chunked(data) {
            ChunkedResult::Complete(body, _) => {
                assert_eq!(body, b"Hello World");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn chunked_empty_body() {
        let data = b"0\r\n\r\n";
        match read_body_chunked(data) {
            ChunkedResult::Complete(body, consumed) => {
                assert!(body.is_empty());
                assert_eq!(consumed, data.len());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn chunked_needs_more_on_incomplete_size_line() {
        // No \r\n after the chunk size.
        let data = b"5";
        assert!(matches!(read_body_chunked(data), ChunkedResult::NeedsMore));
    }

    #[test]
    fn chunked_needs_more_on_incomplete_data() {
        // Size says 5, but only 3 bytes of data.
        let data = b"5\r\nHel";
        assert!(matches!(read_body_chunked(data), ChunkedResult::NeedsMore));
    }

    #[test]
    fn chunked_needs_more_on_missing_final_crlf() {
        // Terminator chunk present but no trailing \r\n.
        let data = b"5\r\nHello\r\n0\r\n";
        assert!(matches!(read_body_chunked(data), ChunkedResult::NeedsMore));
    }

    #[test]
    fn chunked_invalid_hex_size() {
        let data = b"ZZ\r\ndata\r\n0\r\n\r\n";
        assert!(matches!(
            read_body_chunked(data),
            ChunkedResult::Error(ParseError::BadChunkSize(_))
        ));
    }

    #[test]
    fn chunked_missing_crlf_after_data() {
        // Size 5, data "Hello", but no \r\n trailing the data.
        let data = b"5\r\nHello0\r\n\r\n";
        assert!(matches!(
            read_body_chunked(data),
            ChunkedResult::Error(ParseError::BadChunkSize(_))
        ));
    }

    #[test]
    fn chunked_with_extension_ignored() {
        // Chunk extensions after ';' must be stripped.
        let data = b"5;name=value\r\nHello\r\n0\r\n\r\n";
        match read_body_chunked(data) {
            ChunkedResult::Complete(body, _) => assert_eq!(body, b"Hello"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn chunked_uppercase_hex() {
        // "A" = 10 bytes
        let data = b"A\r\n0123456789\r\n0\r\n\r\n";
        match read_body_chunked(data) {
            ChunkedResult::Complete(body, _) => assert_eq!(body.len(), 10),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- memchr_crlf -------------------------------------------------------

    #[test]
    fn memchr_finds_first_crlf() {
        assert_eq!(super::memchr_crlf(b"abc\r\ndef"), Some(3));
        assert_eq!(super::memchr_crlf(b"\r\n"),       Some(0));
        assert_eq!(super::memchr_crlf(b"noend"),      None);
    }
}