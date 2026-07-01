//! Configuration loading, parsing, and validation.
//!
//! # Usage
//!
//! ```rust,no_run
//! use crate::config;
//!
//! let servers = config::load("path/to/server.conf").expect("bad config");
//! // servers is Vec<ServerConfig>, validated and ready to use.
//! ```
pub mod parser;
pub mod types;
pub mod validator;

use types::{ConfigError, ServerConfig};

/// Convenience function: parse the file at `path` and validate all configs.
///
/// This is the single call sites (e.g. `main`) should use; they should not
/// need to call `parser` and `validator` separately.
pub fn load(path: &str) -> Result<Vec<ServerConfig>, ConfigError> {
    let configs = parser::parse_file(path)?;
    validator::validate(&configs)?;
    Ok(configs)
}