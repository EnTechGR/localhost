/// Config file parser.
///
/// # Format overview
///
/// The config file consists of one or more `server { }` blocks. Each server
/// block may contain `route { }` sub-blocks and bare key–value directives.
/// Comments are not supported (per spec). Whitespace is ignored.
///
/// ```text
/// server {
///     host        127.0.0.1
///     port        8080 8443
///     server_name example.com www.example.com
///     client_body_limit 5242880
///     error_page  404 /var/www/errors/404.html
///     error_page  500 /var/www/errors/500.html
///
///     route / {
///         methods         GET HEAD
///         root            /var/www/html
///         index           index.html
///         directory_listing off
///     }
///
///     route /upload {
///         methods         POST
///         root            /var/www/uploads
///         client_body_limit 10485760
///     }
///
///     route /old {
///         redirect        301 /new
///     }
///
///     route /cgi-bin {
///         methods         GET POST
///         root            /var/www/cgi-bin
///         cgi             .py /usr/bin/python3
///         cgi             .php /usr/bin/php-cgi
///     }
/// }
/// ```
///
/// # Parsing strategy
///
/// 1. Read the whole file into a `String`.
/// 2. Tokenise into a flat `Vec<Token>` (words + `{` + `}`).
/// 3. Walk the token stream with a recursive-descent parser.
///    - `parse_file`         → loop over top-level `server` blocks
///    - `parse_server_block` → directives + nested `route` blocks
///    - `parse_route_block`  → directives inside a route
use std::collections::HashMap;

use crate::config::types::{ConfigError, Method, Redirect, RouteConfig, ServerConfig};

// ---------------------------------------------------------------------------
// Tokeniser
// ---------------------------------------------------------------------------

/// A minimal token: either a `{`, a `}`, or an arbitrary word.
/// We track the 1-based line number for error messages.
#[derive(Debug, Clone)]
struct Token {
    value: String,
    line:  usize,
}

impl Token {
    fn is_open_brace(&self)  -> bool { self.value == "{" }
    fn is_close_brace(&self) -> bool { self.value == "}" }
}

/// Tokenise the raw config text.
///
/// Splits on ASCII whitespace; `{` and `}` may be attached to words or
/// stand alone – they are always split into their own tokens.
fn tokenise(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    for (line_idx, line) in source.lines().enumerate() {
        let line_no = line_idx + 1;
        for raw_word in line.split_whitespace() {
            // Expand `word{` or `}word` etc. so braces are always solo tokens.
            let mut buf = String::new();
            for ch in raw_word.chars() {
                if ch == '{' || ch == '}' {
                    if !buf.is_empty() {
                        tokens.push(Token { value: buf.clone(), line: line_no });
                        buf.clear();
                    }
                    tokens.push(Token { value: ch.to_string(), line: line_no });
                } else {
                    buf.push(ch);
                }
            }
            if !buf.is_empty() {
                tokens.push(Token { value: buf, line: line_no });
            }
        }
    }
    tokens
}

// ---------------------------------------------------------------------------
// Token stream helper
// ---------------------------------------------------------------------------

struct TokenStream<'a> {
    tokens: &'a [Token],
    pos:    usize,
}

impl<'a> TokenStream<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        TokenStream { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<&'a Token> {
        let t = self.tokens.get(self.pos);
        if t.is_some() { self.pos += 1; }
        t
    }

    /// Consume the next token and return its value. Error if stream is empty.
    fn expect_word(&mut self, context: &'static str) -> Result<&'a Token, ConfigError> {
        match self.next() {
            Some(t) => Ok(t),
            None => Err(ConfigError::MissingDirective {
                directive: context,
                context:   "end of file".into(),
            }),
        }
    }

    /// Expect the next token to be `{`.
    fn expect_open(&mut self, block: &str) -> Result<(), ConfigError> {
        match self.next() {
            Some(t) if t.is_open_brace() => Ok(()),
            Some(t) => Err(ConfigError::MalformedBlock {
                detail: format!("expected '{{' to open {block} block, got '{}'", t.value),
                line:   t.line,
            }),
            None => Err(ConfigError::MalformedBlock {
                detail: format!("unexpected end of file, expected '{{' for {block}"),
                line:   0,
            }),
        }
    }

    fn current_line(&self) -> usize {
        self.peek().or_else(|| self.tokens.last()).map_or(0, |t| t.line)
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.tokens.len()
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a config file from disk into a list of server configurations.
///
/// Returns `Err(ConfigError)` on any I/O or syntax error.
/// Does **not** validate semantics (e.g. port ranges, path existence) —
/// call [`crate::config::validator::validate`] after this.
pub fn parse_file(path: &str) -> Result<Vec<ServerConfig>, ConfigError> {
    let source = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
    parse_source(&source)
}

/// Parse config from an in-memory string (useful for tests).
pub fn parse_source(source: &str) -> Result<Vec<ServerConfig>, ConfigError> {
    let tokens = tokenise(source);
    let mut stream = TokenStream::new(&tokens);
    let mut servers = Vec::new();

    while !stream.is_empty() {
        let token = stream.expect_word("server")?;
        if token.value != "server" {
            return Err(ConfigError::UnknownDirective {
                name: token.value.clone(),
                line: token.line,
            });
        }
        servers.push(parse_server_block(&mut stream)?);
    }

    Ok(servers)
}

// ---------------------------------------------------------------------------
// Server block
// ---------------------------------------------------------------------------

/// Parse one `server { ... }` block from the token stream.
///
/// The `server` keyword has already been consumed. This function consumes
/// the opening `{`, all directives and sub-blocks, and the closing `}`.
fn parse_server_block(stream: &mut TokenStream<'_>) -> Result<ServerConfig, ConfigError> {
    stream.expect_open("server")?;

    let mut cfg = ServerConfig::default();

    loop {
        let token = match stream.peek() {
            None => {
                return Err(ConfigError::MalformedBlock {
                    detail: "unexpected end of file inside server block".into(),
                    line:   stream.current_line(),
                })
            }
            Some(t) => t,
        };

        if token.is_close_brace() {
            stream.next(); // consume `}`
            break;
        }

        let directive = stream.next().unwrap(); // safe: peeked above
        match directive.value.as_str() {
            "host" => {
                let val = stream.expect_word("host value")?;
                cfg.host = val.value.clone();
            }

            "port" | "listen" => {
                // Accept one or more ports on the same line.
                // Stop when we see a brace, a known directive keyword, or a
                // non-numeric token (which would be a parse error anyway).
                let line = directive.line;
                let first = stream.expect_word("port value")?;
                let port = parse_port(&first.value, line)?;
                cfg.ports.push(port);
                // Greedily consume additional port numbers on the same source line.
                while let Some(t) = stream.peek() {
                    if t.line != first.line { break; }
                    if t.is_open_brace() || t.is_close_brace() { break; }
                    // Stop if the next token looks like a directive keyword
                    // (i.e. is not parseable as a number).
                    if t.value.parse::<u32>().is_err() { break; }
                    let t = stream.next().unwrap();
                    cfg.ports.push(parse_port(&t.value, t.line)?);
                }
            }

            "server_name" => {
                // One or more names on the same source line.
                let first = stream.expect_word("server_name value")?;
                let first_line = first.line;
                cfg.server_names.push(first.value.clone());
                while let Some(t) = stream.peek() {
                    if t.line != first_line { break; }
                    if t.is_open_brace() || t.is_close_brace() { break; }
                    let t = stream.next().unwrap();
                    cfg.server_names.push(t.value.clone());
                }
            }

            "client_body_limit" => {
                let val = stream.expect_word("client_body_limit value")?;
                cfg.client_body_limit = parse_size(&val.value, val.line)?;
            }

            "error_page" => {
                let code_tok = stream.expect_word("error_page status code")?;
                let path_tok = stream.expect_word("error_page file path")?;
                let code = parse_status_code(&code_tok.value, code_tok.line)?;
                cfg.error_pages.insert(code, path_tok.value.clone());
            }

            "route" => {
                // route <path> { ... }
                let path_tok = stream.expect_word("route path")?;
                let route_path = path_tok.value.clone();
                let route = parse_route_block(stream, route_path)?;
                cfg.routes.push(route);
            }

            other => {
                return Err(ConfigError::UnknownDirective {
                    name: other.to_string(),
                    line: directive.line,
                });
            }
        }
    }

    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Route block
// ---------------------------------------------------------------------------

/// Parse one `route <path> { ... }` block.
///
/// The `route` keyword and path token have already been consumed.
/// This function consumes `{`, all directives, and `}`.
fn parse_route_block(
    stream:     &mut TokenStream<'_>,
    route_path: String,
) -> Result<RouteConfig, ConfigError> {
    stream.expect_open("route")?;

    let mut cfg = RouteConfig {
        path: route_path,
        ..Default::default()
    };

    let mut seen: HashMap<&'static str, usize> = HashMap::new();

    loop {
        let token = match stream.peek() {
            None => {
                return Err(ConfigError::MalformedBlock {
                    detail: "unexpected end of file inside route block".into(),
                    line:   stream.current_line(),
                })
            }
            Some(t) => t,
        };

        if token.is_close_brace() {
            stream.next();
            break;
        }

        let directive = stream.next().unwrap();
        match directive.value.as_str() {
            "methods" | "method" => {
                let first = stream.expect_word("methods value")?;
                let first_line = first.line;
                cfg.methods.push(parse_method(&first.value, first.line)?);
                while let Some(t) = stream.peek() {
                    if t.line != first_line { break; }
                    if t.is_open_brace() || t.is_close_brace() { break; }
                    let t = stream.next().unwrap();
                    cfg.methods.push(parse_method(&t.value, t.line)?);
                }
            }

            "root" => {
                mark_once(&mut seen, "root", directive.line)?;
                let val = stream.expect_word("root path")?;
                cfg.root = Some(val.value.clone());
            }
            "index" | "default_file" => {
                mark_once(&mut seen, "index", directive.line)?;
                let val = stream.expect_word("index file")?;
                cfg.index_file = Some(val.value.clone());
            }
            "directory_listing" => {
                mark_once(&mut seen, "directory_listing", directive.line)?;
                let val = stream.expect_word("directory_listing value")?;
                cfg.directory_listing = parse_bool(&val.value, val.line)?;
            }
            "redirect" => {
                mark_once(&mut seen, "redirect", directive.line)?;
                let code_tok   = stream.expect_word("redirect status code")?;
                let target_tok = stream.expect_word("redirect target URL")?;
                let code = parse_redirect_code(&code_tok.value, code_tok.line)?;
                cfg.redirect = Some(Redirect {
                    code,
                    target: target_tok.value.clone(),
                });
            }
            "client_body_limit" => {
                mark_once(&mut seen, "client_body_limit", directive.line)?;
                let val = stream.expect_word("client_body_limit value")?;
                cfg.client_body_limit = Some(parse_size(&val.value, val.line)?);
            }

            "cgi" => {
                // cgi <.ext> <interpreter-path>
                let ext_tok = stream.expect_word("cgi extension")?;
                let interp_tok = stream.expect_word("cgi interpreter path")?;
                let ext = ext_tok.value.trim_start_matches('.').to_lowercase();
                cfg.cgi_extensions.insert(ext, interp_tok.value.clone());
            }

            other => {
                return Err(ConfigError::UnknownDirective {
                    name: other.to_string(),
                    line: directive.line,
                });
            }
        }
    }

    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Small parsing helpers
// ---------------------------------------------------------------------------

fn parse_port(s: &str, line: usize) -> Result<u16, ConfigError> {
    s.parse::<u32>()
        .map_err(|_| ConfigError::InvalidValue {
            field:  "port",
            value:  s.to_string(),
            reason: "must be a positive integer",
        })
        .and_then(|n| {
            if n == 0 || n > 65535 {
                Err(ConfigError::InvalidPort(n))
            } else {
                Ok(n as u16)
            }
        })
        // attach line via re-mapping if we need it in the future
        .map_err(|e| match e {
            ConfigError::InvalidValue { .. } => ConfigError::MalformedBlock {
                detail: format!("invalid port '{s}'"),
                line,
            },
            other => other,
        })
}

fn parse_status_code(s: &str, line: usize) -> Result<u16, ConfigError> {
    s.parse::<u16>().map_err(|_| ConfigError::MalformedBlock {
        detail: format!("invalid HTTP status code '{s}'"),
        line,
    })
}

fn parse_redirect_code(s: &str, line: usize) -> Result<u16, ConfigError> {
    let code = parse_status_code(s, line)?;
    match code {
        301 | 302 | 307 | 308 => Ok(code),
        _ => Err(ConfigError::InvalidValue {
            field:  "redirect code",
            value:  s.to_string(),
            reason: "must be one of 301, 302, 307, 308",
        }),
    }
}

fn parse_method(s: &str, line: usize) -> Result<Method, ConfigError> {
    Method::from_str(s).ok_or_else(|| ConfigError::MalformedBlock {
        detail: format!("unknown HTTP method '{s}'"),
        line,
    })
}

fn parse_bool(s: &str, line: usize) -> Result<bool, ConfigError> {
    match s {
        "on" | "true"  | "yes" | "1" => Ok(true),
        "off"| "false" | "no"  | "0" => Ok(false),
        _ => Err(ConfigError::MalformedBlock {
            detail: format!("expected on/off for boolean, got '{s}'"),
            line,
        }),
    }
}

/// Parse a byte-size value. Accepts raw numbers or numbers with `k`/`m`/`g`
/// suffix (case-insensitive), e.g. `1048576`, `1m`, `512k`.
fn parse_size(s: &str, line: usize) -> Result<usize, ConfigError> {
    let s_lower = s.to_lowercase();
    let (num_str, multiplier) = if s_lower.ends_with('g') {
        (&s[..s.len() - 1], 1_073_741_824usize)
    } else if s_lower.ends_with('m') {
        (&s[..s.len() - 1], 1_048_576usize)
    } else if s_lower.ends_with('k') {
        (&s[..s.len() - 1], 1_024usize)
    } else {
        (s, 1usize)
    };

    num_str
        .parse::<usize>()
        .map(|n| n * multiplier)
        .map_err(|_| ConfigError::MalformedBlock {
            detail: format!("invalid size '{s}'"),
            line,
        })
}

/// Record a "set-once" directive, erroring if already seen in this block.
fn mark_once(
    seen: &mut HashMap<&'static str, usize>,
    key:  &'static str,
    line: usize,
) -> Result<(), ConfigError> {
    match seen.insert(key, line) {
        Some(first_line) => Err(ConfigError::DuplicateDirective { directive: key, first_line, line }),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- tokeniser ---------------------------------------------------------

    #[test]
    fn tokenise_braces_attached_to_words() {
        let tokens = tokenise("server{");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].value, "server");
        assert_eq!(tokens[1].value, "{");
    }

    #[test]
    fn tokenise_tracks_line_numbers() {
        let src = "server {\n    host 127.0.0.1\n}";
        let tokens = tokenise(src);
        assert_eq!(tokens[0].line, 1); // server
        assert_eq!(tokens[1].line, 1); // {
        assert_eq!(tokens[2].line, 2); // host
        assert_eq!(tokens[3].line, 2); // 127.0.0.1
        assert_eq!(tokens[4].line, 3); // }
    }

    // ---- parse_size --------------------------------------------------------

    #[test]
    fn parse_size_raw_bytes() {
        assert_eq!(parse_size("1048576", 0).unwrap(), 1_048_576);
    }

    #[test]
    fn parse_size_with_suffix() {
        assert_eq!(parse_size("1m", 0).unwrap(),   1_048_576);
        assert_eq!(parse_size("512k", 0).unwrap(), 524_288);
        assert_eq!(parse_size("2M", 0).unwrap(),   2_097_152);
        assert_eq!(parse_size("1G", 0).unwrap(),   1_073_741_824);
    }

    #[test]
    fn parse_size_invalid() {
        assert!(parse_size("abc", 1).is_err());
    }

    // ---- minimal valid config ----------------------------------------------

    #[test]
    fn minimal_server_block() {
        let src = r#"
server {
    host 127.0.0.1
    port 8080

    route / {
        root /var/www/html
    }
}
"#;
        let servers = parse_source(src).expect("should parse");
        assert_eq!(servers.len(), 1);
        let s = &servers[0];
        assert_eq!(s.host, "127.0.0.1");
        assert_eq!(s.ports, vec![8080]);
        assert_eq!(s.routes.len(), 1);
        assert_eq!(s.routes[0].path, "/");
        assert_eq!(s.routes[0].root.as_deref(), Some("/var/www/html"));
    }

    #[test]
    fn multiple_ports_same_line() {
        let src = r#"
server {
    host 0.0.0.0
    port 80 443 8080
    route / { root /www }
}
"#;
        let servers = parse_source(src).unwrap();
        assert_eq!(servers[0].ports, vec![80, 443, 8080]);
    }

    #[test]
    fn multiple_server_blocks() {
        let src = r#"
server { host 127.0.0.1  port 8080  route / { root /a } }
server { host 127.0.0.1  port 9090  route / { root /b } }
"#;
        let servers = parse_source(src).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].ports[0], 8080);
        assert_eq!(servers[1].ports[0], 9090);
    }

    #[test]
    fn error_pages_parsed() {
        let src = r#"
server {
    host 0.0.0.0  port 80
    error_page 404 /errors/404.html
    error_page 500 /errors/500.html
    route / { root /www }
}
"#;
        let servers = parse_source(src).unwrap();
        let pages = &servers[0].error_pages;
        assert_eq!(pages.get(&404).map(String::as_str), Some("/errors/404.html"));
        assert_eq!(pages.get(&500).map(String::as_str), Some("/errors/500.html"));
    }

    #[test]
    fn route_with_redirect() {
        let src = r#"
server {
    host 0.0.0.0  port 80
    route /old { redirect 301 /new }
}
"#;
        let servers = parse_source(src).unwrap();
        let redir = servers[0].routes[0].redirect.as_ref().unwrap();
        assert_eq!(redir.code, 301);
        assert_eq!(redir.target, "/new");
    }

    #[test]
    fn route_with_cgi() {
        let src = r#"
server {
    host 0.0.0.0  port 80
    route /cgi-bin {
        root /usr/lib/cgi-bin
        cgi .py /usr/bin/python3
    }
}
"#;
        let servers = parse_source(src).unwrap();
        let route = &servers[0].routes[0];
        assert_eq!(route.cgi_extensions.get("py").map(String::as_str), Some("/usr/bin/python3"));
    }

    #[test]
    fn route_methods_parsed() {
        let src = r#"
server {
    host 0.0.0.0  port 80
    route /upload {
        methods POST DELETE
        root /uploads
    }
}
"#;
        let servers = parse_source(src).unwrap();
        let methods = &servers[0].routes[0].methods;
        assert!(methods.contains(&Method::Post));
        assert!(methods.contains(&Method::Delete));
        assert!(!methods.contains(&Method::Get));
    }

    #[test]
    fn directory_listing_toggle() {
        let src = r#"
server {
    host 0.0.0.0  port 80
    route /files { root /files  directory_listing on }
}
"#;
        let servers = parse_source(src).unwrap();
        assert!(servers[0].routes[0].directory_listing);
    }

    #[test]
    fn client_body_limit_per_route() {
        let src = r#"
server {
    host 0.0.0.0  port 80
    route /upload { root /up  client_body_limit 10m }
}
"#;
        let servers = parse_source(src).unwrap();
        assert_eq!(servers[0].routes[0].client_body_limit, Some(10 * 1_048_576));
    }

    #[test]
    fn server_names_multiple() {
        let src = r#"
server {
    host 0.0.0.0  port 80
    server_name example.com www.example.com
    route / { root /www }
}
"#;
        let servers = parse_source(src).unwrap();
        assert_eq!(servers[0].server_names, vec!["example.com", "www.example.com"]);
    }

    // ---- error cases -------------------------------------------------------

    #[test]
    fn unknown_top_level_directive_is_error() {
        let src = "garbage { }";
        assert!(parse_source(src).is_err());
    }

    #[test]
    fn unclosed_server_block_is_error() {
        let src = "server { host 0.0.0.0  port 80";
        assert!(parse_source(src).is_err());
    }

    #[test]
    fn invalid_redirect_code_is_error() {
        let src = r#"server { host 0.0.0.0 port 80 route /x { redirect 200 /y } }"#;
        assert!(parse_source(src).is_err());
    }

    #[test]
    fn invalid_port_zero_is_error() {
        let src = "server { host 0.0.0.0  port 0  route / { root /w } }";
        assert!(parse_source(src).is_err());
    }

    #[test]
    fn invalid_port_too_large_is_error() {
        let src = "server { host 0.0.0.0  port 99999  route / { root /w } }";
        assert!(parse_source(src).is_err());
    }
    #[test]
    fn duplicate_root_in_route_is_rejected() {
        let src = "server { host 0.0.0.0 port 80 route / { root /a  root /b } }";
        assert!(matches!(parse_source(src).unwrap_err(),
            ConfigError::DuplicateDirective { directive: "root", .. }));
    }

    #[test]
    fn index_and_default_file_conflict() {
        let src = "server { host 0.0.0.0 port 80 route / { index a.html  default_file b.html } }";
        assert!(matches!(parse_source(src).unwrap_err(),
            ConfigError::DuplicateDirective { directive: "index", .. }));
    }

    #[test]
    fn duplicate_host_in_server_is_rejected() {
        let src = "server { host 0.0.0.0  host 127.0.0.1  port 80  route / { root /a } }";
        assert!(matches!(parse_source(src).unwrap_err(),
            ConfigError::DuplicateDirective { directive: "host", .. }));
    }

    #[test]
    fn repeated_methods_still_accumulate() {
        let src = "server { host 0.0.0.0 port 80 route / { methods GET  methods POST  root /a } }";
        let s = parse_source(src).unwrap();
        let m = &s[0].routes[0].methods;
        assert!(m.contains(&Method::Get) && m.contains(&Method::Post));
    }
}