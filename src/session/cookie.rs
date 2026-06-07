//! HTTP cookie parsing and generation.
//!
//! Handles two directions:
//! - **Inbound:** parse the `Cookie` request header into name→value pairs.
//! - **Outbound:** build a `Set-Cookie` response header with configurable
//!   attributes (Path, Max-Age, HttpOnly, SameSite).
//!
//! The session layer uses a single cookie named [`SESSION_COOKIE`] whose
//! value is the opaque [`SessionId`](super::SessionId).
use std::collections::HashMap;

/// Name of the session cookie set by the server.
pub const SESSION_COOKIE: &str = "SID";

// ---------------------------------------------------------------------------
// CookieOptions
// ---------------------------------------------------------------------------

/// Attributes appended to a `Set-Cookie` header.
#[derive(Debug, Clone)]
pub struct CookieOptions {
    /// Cookie `Path` attribute (default `"/"`).
    pub path: String,
    /// `Max-Age` in seconds. `None` makes the cookie session-scoped (deleted
    /// when the browser closes).
    pub max_age: Option<u64>,
    /// Whether to set the `HttpOnly` flag (prevents JavaScript access).
    pub http_only: bool,
    /// `SameSite` attribute: `"Strict"`, `"Lax"`, or `"None"`.
    pub same_site: Option<String>,
}

impl Default for CookieOptions {
    fn default() -> Self {
        CookieOptions {
            path: "/".into(),
            max_age: Some(3600),
            http_only: true,
            same_site: Some("Lax".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a `Cookie` header value into name→value pairs.
///
/// Format (RFC 6265 §4.2): `name1=value1; name2=value2; …`
///
/// Handles leading/trailing whitespace, missing values (name with no `=` is
/// ignored), and empty pairs (consecutive semicolons).
pub fn parse_cookies(header: &str) -> HashMap<String, String> {
    let mut cookies = HashMap::new();
    for pair in header.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((name, value)) = pair.split_once('=') {
            let name = name.trim();
            let value = value.trim();
            if !name.is_empty() {
                cookies.insert(name.to_string(), value.to_string());
            }
        }
    }
    cookies
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

/// Build a `Set-Cookie` header **value** (without the `Set-Cookie:` prefix).
///
/// Example output: `SID=abc123; Path=/; Max-Age=3600; HttpOnly; SameSite=Lax`
pub fn set_cookie_header(name: &str, value: &str, opts: &CookieOptions) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(5);
    parts.push(format!("{name}={value}"));
    parts.push(format!("Path={}", opts.path));
    if let Some(max_age) = opts.max_age {
        parts.push(format!("Max-Age={max_age}"));
    }
    if opts.http_only {
        parts.push("HttpOnly".into());
    }
    if let Some(ref same_site) = opts.same_site {
        parts.push(format!("SameSite={same_site}"));
    }
    parts.join("; ")
}

// ---------------------------------------------------------------------------
// Session ID extraction
// ---------------------------------------------------------------------------

/// Look up the session cookie in the parsed cookie map.
///
/// Returns the raw session ID string if the `SID` cookie is present and
/// non-empty, or `None` otherwise.
pub fn extract_session_id(cookies: &HashMap<String, String>) -> Option<&str> {
    cookies
        .get(SESSION_COOKIE)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_cookies ----------------------------------------------------

    #[test]
    fn parses_standard_cookie_header() {
        let cookies = parse_cookies("SID=abc123; theme=dark; lang=en");
        assert_eq!(cookies.get("SID").map(String::as_str), Some("abc123"));
        assert_eq!(cookies.get("theme").map(String::as_str), Some("dark"));
        assert_eq!(cookies.get("lang").map(String::as_str), Some("en"));
        assert_eq!(cookies.len(), 3);
    }

    #[test]
    fn handles_extra_whitespace() {
        let cookies = parse_cookies("  SID = abc ;  theme=dark  ");
        assert_eq!(cookies.get("SID").map(String::as_str), Some("abc"));
        assert_eq!(cookies.get("theme").map(String::as_str), Some("dark"));
    }

    #[test]
    fn handles_empty_and_malformed_pairs() {
        let cookies = parse_cookies("; ; name=val; noequalssign; =emptyname;");
        assert_eq!(cookies.get("name").map(String::as_str), Some("val"));
        // "noequalssign" has no '=' → skipped.
        // "=emptyname" has empty name → skipped.
        assert_eq!(cookies.len(), 1);
    }

    #[test]
    fn empty_string_returns_empty_map() {
        assert!(parse_cookies("").is_empty());
    }

    #[test]
    fn value_with_equals_sign() {
        // Values may contain '='; split_once only splits at the first '='.
        let cookies = parse_cookies("data=a=b=c");
        assert_eq!(cookies.get("data").map(String::as_str), Some("a=b=c"));
    }

    // ---- set_cookie_header ------------------------------------------------

    #[test]
    fn full_options() {
        let opts = CookieOptions::default();
        let header = set_cookie_header("SID", "xyz", &opts);
        assert!(header.starts_with("SID=xyz"));
        assert!(header.contains("Path=/"));
        assert!(header.contains("Max-Age=3600"));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("SameSite=Lax"));
    }

    #[test]
    fn session_scoped_cookie() {
        let opts = CookieOptions {
            max_age: None,
            http_only: false,
            same_site: None,
            ..Default::default()
        };
        let header = set_cookie_header("SID", "abc", &opts);
        assert_eq!(header, "SID=abc; Path=/");
    }

    // ---- extract_session_id -----------------------------------------------

    #[test]
    fn extracts_present_session_id() {
        let cookies = parse_cookies("SID=hello; other=world");
        assert_eq!(extract_session_id(&cookies), Some("hello"));
    }

    #[test]
    fn returns_none_when_absent() {
        let cookies = parse_cookies("other=world");
        assert_eq!(extract_session_id(&cookies), None);
    }

    #[test]
    fn returns_none_for_empty_value() {
        let cookies = parse_cookies("SID=");
        assert_eq!(extract_session_id(&cookies), None);
    }
}