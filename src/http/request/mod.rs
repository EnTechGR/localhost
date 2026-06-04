//! HTTP request parsing.
//!
//! - [`types`]  — `Request`, `HeaderMap`, `Version`, `ParseError`
//! - [`parser`] — incremental header parser
//! - [`body`]   — fixed-length and chunked body decoding
pub mod body;
pub mod parser;
pub mod types;