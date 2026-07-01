/// Semantic validation of parsed server configurations.
///
/// This module runs **after** `parser::parse_file` and checks invariants that
/// the parser cannot detect (wrong types, duplicate bindings, missing files).
///
/// Validation is intentionally strict so that operators get clear error
/// messages at startup rather than silent misbehaviour at runtime.
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::types::{ConfigError, ServerConfig};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Validate a slice of parsed server configurations.
///
/// Checks performed (in order):
/// 1. Every server has at least one port.
/// 2. No two servers share the same `host:port` pair without a distinct
///    `server_name` to discriminate between them.
/// 3. All referenced error-page files exist on disk.
/// 4. Every route that defines a `root` points at a directory that exists.
/// 5. Redirect routes have a non-empty target.
/// 6. CGI interpreter paths are absolute.
///
/// Returns the first `ConfigError` encountered, or `Ok(())`.
pub fn validate(configs: &[ServerConfig]) -> Result<(), ConfigError> {
    check_non_empty_ports(configs)?;
    check_duplicate_host_ports(configs)?;
    check_error_pages(configs)?;
    check_route_roots(configs)?;
    check_redirects(configs)?;
    check_cgi_interpreters(configs)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

/// Every server block must declare at least one port.
fn check_non_empty_ports(configs: &[ServerConfig]) -> Result<(), ConfigError> {
    for (idx, server) in configs.iter().enumerate() {
        if server.ports.is_empty() {
            return Err(ConfigError::MissingDirective {
                directive: "port",
                context:   format!("server block #{} (host: {})", idx + 1, server.host),
            });
        }
    }
    Ok(())
}

/// Detect ambiguous virtual-host bindings.
///
/// Two servers may share a `host:port` only if *both* have at least one
/// `server_name` entry and their `server_name` sets are disjoint. If either
/// server has no names, traffic cannot be unambiguously routed.
fn check_duplicate_host_ports(configs: &[ServerConfig]) -> Result<(), ConfigError> {
    // host:port → list of server indices that claim it
    let mut bindings: HashMap<(String, u16), Vec<usize>> = HashMap::new();

    for (idx, server) in configs.iter().enumerate() {
        for &port in &server.ports {
            bindings
                .entry((server.host.clone(), port))
                .or_default()
                .push(idx);
        }
    }

    for ((host, port), indices) in &bindings {
        if indices.len() < 2 {
            continue;
        }
        // Multiple servers on the same host:port — each must have ≥1 server_name.
        for &idx in indices {
            if configs[idx].server_names.is_empty() {
                return Err(ConfigError::DuplicateHostPort {
                    host: host.clone(),
                    port: *port,
                });
            }
        }
        // All have names — verify they are pairwise disjoint.
        let mut seen_names: HashSet<&str> = HashSet::new();
        for &idx in indices {
            for name in &configs[idx].server_names {
                if !seen_names.insert(name.as_str()) {
                    return Err(ConfigError::DuplicateHostPort {
                        host: host.clone(),
                        port: *port,
                    });
                }
            }
        }
    }

    Ok(())
}

/// All `error_page` file paths must exist on disk.
///
/// This check is skipped in test builds (cfg(test)) because unit tests run
/// without a real file system layout.
fn check_error_pages(configs: &[ServerConfig]) -> Result<(), ConfigError> {
    #[cfg(not(test))]
    for server in configs {
        for (&code, path) in &server.error_pages {
            if !Path::new(path).is_file() {
                return Err(ConfigError::ErrorPageNotFound {
                    code,
                    path: path.clone(),
                });
            }
        }
    }
    // In test mode we skip filesystem checks so tests don't need real files.
    #[cfg(test)]
    let _ = configs;
    Ok(())
}

/// Route `root` values must point at existing directories.
///
/// Like `check_error_pages`, filesystem checks are skipped in test mode.
fn check_route_roots(configs: &[ServerConfig]) -> Result<(), ConfigError> {
    #[cfg(not(test))]
    for server in configs {
        for route in &server.routes {
            if let Some(root) = &route.root {
                if !Path::new(root).is_dir() {
                    return Err(ConfigError::InvalidRoot { path: root.clone() });
                }
            }
        }
    }
    #[cfg(test)]
    let _ = configs;
    Ok(())
}

/// Every redirect must have a non-empty target URL.
fn check_redirects(configs: &[ServerConfig]) -> Result<(), ConfigError> {
    for server in configs {
        for route in &server.routes {
            if let Some(redir) = &route.redirect {
                if redir.target.trim().is_empty() {
                    return Err(ConfigError::InvalidValue {
                        field:  "redirect target",
                        value:  String::new(),
                        reason: "redirect target URL must not be empty",
                    });
                }
            }
        }
    }
    Ok(())
}

/// CGI interpreter paths must be absolute (start with `/`).
///
/// Relative paths are dangerous because the server's working directory is
/// unpredictable at runtime.
fn check_cgi_interpreters(configs: &[ServerConfig]) -> Result<(), ConfigError> {
    for server in configs {
        for route in &server.routes {
            for (ext, interp) in &route.cgi_extensions {
                if !interp.starts_with('/') {
                    return Err(ConfigError::InvalidValue {
                        field:  "cgi interpreter",
                        value:  interp.clone(),
                        reason: "CGI interpreter path must be absolute (start with '/')",
                    });
                }
                // In non-test mode, also verify the interpreter exists.
                #[cfg(not(test))]
                if !Path::new(interp).is_file() {
                    return Err(ConfigError::InvalidValue {
                        field:  "cgi interpreter",
                        value:  interp.clone(),
                        reason: "CGI interpreter not found on disk",
                    });
                }
                let _ = ext; // suppress unused warning in test mode
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{ Redirect, RouteConfig, ServerConfig};

    fn minimal_server(host: &str, ports: &[u16]) -> ServerConfig {
        ServerConfig {
            host:   host.into(),
            ports:  ports.to_vec(),
            routes: vec![RouteConfig {
                path: "/".into(),
                root: Some("/var/www".into()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    // ---- port presence -----------------------------------------------------

    #[test]
    fn no_ports_is_error() {
        let cfg = ServerConfig { ports: vec![], ..minimal_server("0.0.0.0", &[]) };
        assert!(validate(&[cfg]).is_err());
    }

    #[test]
    fn single_port_ok() {
        let cfg = minimal_server("0.0.0.0", &[8080]);
        assert!(validate(&[cfg]).is_ok());
    }

    // ---- duplicate host:port -----------------------------------------------

    #[test]
    fn two_servers_same_port_no_server_name_is_error() {
        let a = minimal_server("0.0.0.0", &[80]);
        let b = minimal_server("0.0.0.0", &[80]);
        assert!(validate(&[a, b]).is_err());
    }

    #[test]
    fn two_servers_same_port_distinct_server_names_ok() {
        let mut a = minimal_server("0.0.0.0", &[80]);
        a.server_names = vec!["foo.com".into()];
        let mut b = minimal_server("0.0.0.0", &[80]);
        b.server_names = vec!["bar.com".into()];
        assert!(validate(&[a, b]).is_ok());
    }

    #[test]
    fn two_servers_same_port_overlapping_server_names_is_error() {
        let mut a = minimal_server("0.0.0.0", &[80]);
        a.server_names = vec!["foo.com".into()];
        let mut b = minimal_server("0.0.0.0", &[80]);
        b.server_names = vec!["foo.com".into(), "bar.com".into()];
        assert!(validate(&[a, b]).is_err());
    }

    #[test]
    fn two_servers_different_ports_ok() {
        let a = minimal_server("0.0.0.0", &[80]);
        let b = minimal_server("0.0.0.0", &[443]);
        assert!(validate(&[a, b]).is_ok());
    }

    #[test]
    fn two_servers_different_hosts_same_port_ok() {
        let a = minimal_server("127.0.0.1", &[80]);
        let b = minimal_server("192.168.1.1", &[80]);
        assert!(validate(&[a, b]).is_ok());
    }

    // ---- redirect target ---------------------------------------------------

    #[test]
    fn empty_redirect_target_is_error() {
        let mut server = minimal_server("0.0.0.0", &[80]);
        server.routes[0].redirect = Some(Redirect { code: 301, target: "   ".into() });
        assert!(validate(&[server]).is_err());
    }

    #[test]
    fn valid_redirect_ok() {
        let mut server = minimal_server("0.0.0.0", &[80]);
        server.routes[0].redirect = Some(Redirect { code: 301, target: "/new-path".into() });
        assert!(validate(&[server]).is_ok());
    }

    // ---- CGI interpreter ---------------------------------------------------

    #[test]
    fn relative_cgi_path_is_error() {
        let mut server = minimal_server("0.0.0.0", &[80]);
        server.routes[0].cgi_extensions.insert("py".into(), "python3".into());
        assert!(validate(&[server]).is_err());
    }

    #[test]
    fn absolute_cgi_path_ok_in_test_mode() {
        // In test mode we skip the is_file() check, so any absolute path passes.
        let mut server = minimal_server("0.0.0.0", &[80]);
        server.routes[0]
            .cgi_extensions
            .insert("py".into(), "/usr/bin/python3".into());
        assert!(validate(&[server]).is_ok());
    }

    // ---- combined parse + validate -----------------------------------------

    #[test]
    fn parse_then_validate_valid_config() {
        use crate::config::parser::parse_source;
        let src = r#"
server {
    host 127.0.0.1
    port 8080
    server_name localhost
    error_page 404 /errors/404.html
    route / {
        methods GET HEAD
        root    /var/www/html
        index   index.html
    }
}
"#;
        let configs = parse_source(src).expect("should parse");
        // Validation in test mode skips filesystem checks.
        assert!(validate(&configs).is_ok());
    }
}