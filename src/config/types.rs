use std::collections::HashMap;

// ---------------------------------------------------------------------------
// HTTP Method
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Delete,
    Head,
    Put,
    Options,
}

impl Method {
    /// Parse from a raw string slice (case-sensitive per RFC 9110).
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "GET"     => Some(Method::Get),
            "POST"    => Some(Method::Post),
            "DELETE"  => Some(Method::Delete),
            "HEAD"    => Some(Method::Head),
            "PUT"     => Some(Method::Put),
            "OPTIONS" => Some(Method::Options),
            _         => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Get     => "GET",
            Method::Post    => "POST",
            Method::Delete  => "DELETE",
            Method::Head    => "HEAD",
            Method::Put     => "PUT",
            Method::Options => "OPTIONS",
        }
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Redirect
// ---------------------------------------------------------------------------

/// An HTTP redirect instruction attached to a route.
#[derive(Debug, Clone)]
pub struct Redirect {
    /// Must be a valid redirect status code: 301, 302, 307, or 308.
    pub code: u16,
    /// The Location header value sent to the client.
    pub target: String,
}

// ---------------------------------------------------------------------------
// RouteConfig
// ---------------------------------------------------------------------------

/// Configuration for a single URL-prefix route within a server block.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    /// URL prefix this route matches (e.g. `/`, `/static`, `/api`).
    pub path: String,

    /// HTTP methods explicitly allowed on this route.
    /// If empty, all methods are permitted (defaults to server behaviour).
    pub methods: Vec<Method>,

    /// If set, any request matching this route is immediately redirected.
    /// Takes precedence over all other route behaviour.
    pub redirect: Option<Redirect>,

    /// Filesystem root directory to resolve URL paths against.
    /// E.g. root = `/var/www/html`, URL `/foo.html` → `/var/www/html/foo.html`.
    pub root: Option<String>,

    /// File to serve when the resolved path is a directory and no index is
    /// found. Relative to the resolved directory (e.g. `index.html`).
    pub default_file: Option<String>,

    /// Map of file extension → CGI interpreter path.
    /// E.g. `{ "py" => "/usr/bin/python3", "php" => "/usr/bin/php-cgi" }`.
    pub cgi_extensions: HashMap<String, String>,

    /// When `true` and the URL resolves to a directory, emit an HTML listing
    /// of its contents. Disabled by default.
    pub directory_listing: bool,

    /// Alias for `default_file`; kept separate so the config format can use
    /// either `index` or `default_file` as key names.
    pub index_file: Option<String>,

    /// Maximum size in bytes allowed for a client request body on this route.
    /// Overrides the server-level `client_body_limit` when set.
    pub client_body_limit: Option<usize>,
}

impl Default for RouteConfig {
    fn default() -> Self {
        RouteConfig {
            path:              String::from("/"),
            methods:           Vec::new(),
            redirect:          None,
            root:              None,
            default_file:      None,
            cgi_extensions:    HashMap::new(),
            directory_listing: false,
            index_file:        None,
            client_body_limit: None,
        }
    }
}

impl RouteConfig {
    /// Return `true` if the given method is allowed by this route.
    /// An empty `methods` list means "allow all".
    pub fn allows_method(&self, method: &Method) -> bool {
        self.methods.is_empty() || self.methods.contains(method)
    }

    /// Resolve the effective index filename: prefer `index_file`, then
    /// `default_file`, then fall back to `"index.html"`.
    pub fn effective_index(&self) -> &str {
        self.index_file
            .as_deref()
            .or(self.default_file.as_deref())
            .unwrap_or("index.html")
    }

    /// Return the CGI interpreter path for the given file extension, if any.
    pub fn cgi_for_extension(&self, ext: &str) -> Option<&str> {
        self.cgi_extensions.get(ext).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// ServerConfig
// ---------------------------------------------------------------------------

/// Top-level configuration for a single virtual server block.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// IP address or hostname to bind (e.g. `"127.0.0.1"`, `"0.0.0.0"`).
    pub host: String,

    /// One or more TCP ports this server listens on.
    /// Must contain at least one entry after parsing.
    pub ports: Vec<u16>,

    /// Optional `server_name` directives used for virtual-host selection.
    /// The first server for a host:port pair with no matching server_name
    /// acts as the default.
    pub server_names: Vec<String>,

    /// Map of HTTP status code → path to a custom HTML error page file.
    /// E.g. `{ 404 => "/var/www/errors/404.html" }`.
    pub error_pages: HashMap<u16, String>,

    /// Maximum allowed size of a client request body in bytes.
    /// Requests exceeding this limit receive a 413 response.
    /// Defaults to `1_048_576` (1 MiB) when not specified.
    pub client_body_limit: usize,

    /// Ordered list of route configs. Matched by longest prefix.
    pub routes: Vec<RouteConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            host:              String::from("0.0.0.0"),
            ports:             Vec::new(),
            server_names:      Vec::new(),
            error_pages:       HashMap::new(),
            client_body_limit: 1_048_576, // 1 MiB
            routes:            Vec::new(),
        }
    }
}

impl ServerConfig {
    /// Return the custom error page path for `code`, if configured.
    pub fn error_page(&self, code: u16) -> Option<&str> {
        self.error_pages.get(&code).map(String::as_str)
    }

    /// Effective client body limit: the server-level default, since per-route
    /// overrides are checked separately in the handler.
    pub fn body_limit(&self) -> usize {
        self.client_body_limit
    }
}

// ---------------------------------------------------------------------------
// ConfigError
// ---------------------------------------------------------------------------

/// All errors that can arise during config file parsing or validation.
#[derive(Debug)]
pub enum ConfigError {
    /// The config file could not be read from disk.
    Io(std::io::Error),

    /// A required directive was missing from a block.
    /// `.0` names the missing directive, `.1` gives context.
    MissingDirective { directive: &'static str, context: String },

    /// A directive value could not be parsed.
    /// `.field` names the directive, `.value` is what was found, `.reason`
    /// explains why it is invalid.
    InvalidValue { field: &'static str, value: String, reason: &'static str },

    /// An unknown directive was encountered (strict mode).
    UnknownDirective { name: String, line: usize },

    /// A block was not properly closed or opened.
    MalformedBlock { detail: String, line: usize },

    /// Two or more server blocks share the same host:port without distinct
    /// `server_name` entries, making virtual-host selection ambiguous.
    DuplicateHostPort { host: String, port: u16 },

    /// A referenced error page file does not exist on disk.
    ErrorPageNotFound { code: u16, path: String },

    /// A route `root` directory does not exist or is not a directory.
    InvalidRoot { path: String },

    /// A port number was outside the valid range 1–65535.
    InvalidPort(u32),

    /// A "set-once" directive (e.g. `host`, `root`, `redirect`) appeared more
    /// than once within a single block, making its value ambiguous.
    DuplicateDirective { directive: &'static str, first_line: usize, line: usize },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) =>
                write!(f, "I/O error reading config: {e}"),
            ConfigError::MissingDirective { directive, context } =>
                write!(f, "missing directive '{directive}' in {context}"),
            ConfigError::InvalidValue { field, value, reason } =>
                write!(f, "invalid value for '{field}': '{value}' — {reason}"),
            ConfigError::UnknownDirective { name, line } =>
                write!(f, "unknown directive '{name}' at line {line}"),
            ConfigError::MalformedBlock { detail, line } =>
                write!(f, "malformed block at line {line}: {detail}"),
            ConfigError::DuplicateHostPort { host, port } =>
                write!(f, "duplicate host:port '{host}:{port}' with no distinct server_name"),
            ConfigError::ErrorPageNotFound { code, path } =>
                write!(f, "error page for {code} not found at '{path}'"),
            ConfigError::InvalidRoot { path } =>
                write!(f, "route root '{path}' is not a valid directory"),
            ConfigError::InvalidPort(p) =>
                write!(f, "port {p} is out of valid range 1–65535"),
            ConfigError::DuplicateDirective { directive, first_line, line } =>
                write!(f, "directive '{directive}' is set more than once (line {line}; first seen at line {first_line})"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_round_trip() {
        for s in &["GET", "POST", "DELETE", "HEAD", "PUT", "OPTIONS"] {
            let m = Method::from_str(s).expect("should parse");
            assert_eq!(m.as_str(), *s);
        }
        assert!(Method::from_str("PATCH").is_none());
        assert!(Method::from_str("get").is_none()); // case-sensitive
    }

    #[test]
    fn route_allows_method_empty_means_all() {
        let route = RouteConfig::default();
        assert!(route.allows_method(&Method::Get));
        assert!(route.allows_method(&Method::Delete));
    }

    #[test]
    fn route_allows_method_restricted() {
        let route = RouteConfig {
            methods: vec![Method::Get, Method::Head],
            ..Default::default()
        };
        assert!(route.allows_method(&Method::Get));
        assert!(!route.allows_method(&Method::Post));
    }

    #[test]
    fn route_effective_index_fallback() {
        let route = RouteConfig::default();
        assert_eq!(route.effective_index(), "index.html");

        let route = RouteConfig {
            default_file: Some("home.html".into()),
            ..Default::default()
        };
        assert_eq!(route.effective_index(), "home.html");

        let route = RouteConfig {
            index_file:   Some("start.html".into()),
            default_file: Some("home.html".into()),
            ..Default::default()
        };
        // index_file wins over default_file
        assert_eq!(route.effective_index(), "start.html");
    }

    #[test]
    fn server_default_body_limit() {
        let s = ServerConfig::default();
        assert_eq!(s.body_limit(), 1_048_576);
    }

    #[test]
    fn config_error_display_does_not_panic() {
        let errors: Vec<ConfigError> = vec![
            ConfigError::MissingDirective { directive: "host", context: "server block".into() },
            ConfigError::InvalidValue { field: "port", value: "99999".into(), reason: "out of range" },
            ConfigError::DuplicateHostPort { host: "0.0.0.0".into(), port: 8080 },
            ConfigError::ErrorPageNotFound { code: 404, path: "/missing.html".into() },
            ConfigError::InvalidRoot { path: "/no/such/dir".into() },
            ConfigError::InvalidPort(0),
        ];
        for e in &errors {
            // just ensure Display doesn't panic
            let _ = format!("{e}");
        }
    }
}