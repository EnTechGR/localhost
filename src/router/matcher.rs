/// Route and virtual-host matching.
///
/// Two responsibilities:
/// 1. `select_server` — pick the right `ServerConfig` from the list based on
///    the `Host` request header and the port on which the connection arrived.
/// 2. `match_route`   — pick the best `RouteConfig` within that server using
///    longest-prefix matching.
use crate::config::types::{RouteConfig, ServerConfig};

// ---------------------------------------------------------------------------
// Virtual-host selection
// ---------------------------------------------------------------------------

/// Select the `ServerConfig` that should handle a request.
///
/// Algorithm (mirrors nginx's virtual-host selection):
/// 1. Filter configs that include `port` in their `ports` list.
/// 2. Among those, find one whose `server_names` contains an exact match for
///    `host_header` (case-insensitive).
/// 3. If none matched by name, return the first config for that port
///    (the default server for the `host:port` binding).
/// 4. If no config at all owns `port`, fall back to the very first config in
///    the slice (should not happen after validation, but be defensive).
pub fn select_server<'a>(
    host_header: &str,
    port:        u16,
    configs:     &'a [ServerConfig],
) -> &'a ServerConfig {
    // Strip optional port suffix from the Host header ("example.com:8080").
    let host = host_header.split(':').next().unwrap_or(host_header);

    let port_configs: Vec<&ServerConfig> = configs
        .iter()
        .filter(|c| c.ports.contains(&port))
        .collect();

    if port_configs.is_empty() {
        // No server owns this port — return the global default (first config).
        return &configs[0];
    }

    // Try exact server_name match (case-insensitive).
    for config in &port_configs {
        for name in &config.server_names {
            if name.eq_ignore_ascii_case(host) {
                return config;
            }
        }
    }

    // Wildcard prefix match: "*.example.com" matches "www.example.com".
    for config in &port_configs {
        for name in &config.server_names {
            if let Some(suffix) = name.strip_prefix("*.") {
                if host.ends_with(suffix) {
                    return config;
                }
            }
        }
    }

    // Fall back to first server for this port (the default server).
    port_configs[0]
}

// ---------------------------------------------------------------------------
// Route matching
// ---------------------------------------------------------------------------

/// Find the best-matching `RouteConfig` for `path` within a server.
///
/// Strategy:
/// - Exact match always wins over prefix matches.
/// - Among prefix matches, the longest prefix wins.
/// - If no route matches at all, returns `None`.
///
/// The `path` argument should already be decoded (percent-decoded by the
/// HTTP parser) but not further normalised — route paths in the config
/// are compared literally.
pub fn match_route<'a>(path: &str, routes: &'a [RouteConfig]) -> Option<&'a RouteConfig> {
    // Exact match first.
    if let Some(r) = routes.iter().find(|r| r.path == path) {
        return Some(r);
    }

    // Longest prefix match.
    // A route prefix must be followed by '/' or the path must equal the prefix
    // exactly (handled above) to avoid "/stat" matching "/static".
    routes
        .iter()
        .filter(|r| {
            if r.path == "/" {
                // The root route matches everything.
                return true;
            }
            // Prefix must end at a path segment boundary.
            path.starts_with(r.path.as_str())
                && (path.len() == r.path.len()
                    || path.as_bytes().get(r.path.len()) == Some(&b'/'))
        })
        .max_by_key(|r| r.path.len())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{RouteConfig, ServerConfig};

    // ---- helpers -----------------------------------------------------------

    fn server(host: &str, port: u16, names: &[&str]) -> ServerConfig {
        ServerConfig {
            host:         host.into(),
            ports:        vec![port],
            server_names: names.iter().map(|s| s.to_string()).collect(),
            routes:       vec![route("/")],
            ..Default::default()
        }
    }

    fn route(path: &str) -> RouteConfig {
        RouteConfig {
            path: path.into(),
            root: Some("/var/www".into()),
            ..Default::default()
        }
    }

    // ---- select_server -----------------------------------------------------

    #[test]
    fn selects_by_server_name() {
        let configs = vec![
            server("0.0.0.0", 80, &["foo.com"]),
            server("0.0.0.0", 80, &["bar.com"]),
        ];
        let s = select_server("bar.com", 80, &configs);
        assert_eq!(s.server_names, vec!["bar.com"]);
    }

    #[test]
    fn falls_back_to_first_for_port() {
        let configs = vec![
            server("0.0.0.0", 80, &["foo.com"]),
            server("0.0.0.0", 80, &["bar.com"]),
        ];
        // "unknown.com" matches no server_name → first server for port 80.
        let s = select_server("unknown.com", 80, &configs);
        assert_eq!(s.server_names, vec!["foo.com"]);
    }

    #[test]
    fn server_name_matching_is_case_insensitive() {
        let configs = vec![server("0.0.0.0", 80, &["Example.COM"])];
        let s = select_server("example.com", 80, &configs);
        assert_eq!(s.server_names[0], "Example.COM");
    }

    #[test]
    fn host_header_port_suffix_stripped() {
        // "example.com:8080" should match "example.com".
        let configs = vec![server("0.0.0.0", 8080, &["example.com"])];
        let s = select_server("example.com:8080", 8080, &configs);
        assert_eq!(s.server_names[0], "example.com");
    }

    #[test]
    fn wildcard_server_name_match() {
        let configs = vec![server("0.0.0.0", 80, &["*.example.com"])];
        let s = select_server("www.example.com", 80, &configs);
        assert_eq!(s.server_names[0], "*.example.com");
    }

    #[test]
    fn unknown_port_falls_back_to_first_config() {
        let configs = vec![server("0.0.0.0", 80, &["a.com"])];
        // Port 9999 has no matching config.
        let s = select_server("a.com", 9999, &configs);
        assert_eq!(s.server_names[0], "a.com");
    }

    // ---- match_route -------------------------------------------------------

    #[test]
    fn exact_match_wins_over_prefix() {
        let routes = vec![
            route("/"),
            route("/static"),
            route("/static/img"),
        ];
        let r = match_route("/static/img", &routes).unwrap();
        assert_eq!(r.path, "/static/img");
    }

    #[test]
    fn longest_prefix_wins() {
        let routes = vec![
            route("/"),
            route("/static"),
            route("/static/img"),
        ];
        // "/static/img/logo.png" — longest matching prefix is "/static/img".
        let r = match_route("/static/img/logo.png", &routes).unwrap();
        assert_eq!(r.path, "/static/img");
    }

    #[test]
    fn root_matches_any_path() {
        let routes = vec![route("/")];
        assert!(match_route("/anything/at/all", &routes).is_some());
    }

    #[test]
    fn no_match_returns_none() {
        let routes = vec![route("/api"), route("/static")];
        // "/other" does not match "/api" or "/static".
        assert!(match_route("/other", &routes).is_none());
    }

    #[test]
    fn prefix_does_not_match_partial_segment() {
        // "/stat" must NOT match the route "/static".
        let routes = vec![route("/static")];
        assert!(match_route("/stat", &routes).is_none());
    }

    #[test]
    fn prefix_matches_at_slash_boundary() {
        let routes = vec![route("/static")];
        // "/static/file.css" — matches because boundary is '/'.
        let r = match_route("/static/file.css", &routes).unwrap();
        assert_eq!(r.path, "/static");
    }

    #[test]
    fn empty_routes_returns_none() {
        assert!(match_route("/anything", &[]).is_none());
    }

    #[test]
    fn single_slash_route_matches_root() {
        let routes = vec![route("/")];
        let r = match_route("/", &routes).unwrap();
        assert_eq!(r.path, "/");
    }
}