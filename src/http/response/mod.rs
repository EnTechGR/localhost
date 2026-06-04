//! HTTP response construction and serialisation.
//!
//! - [`types`]   — `Response`, `StatusCode`, `ResponseHeaders`
//! - [`builder`] — factory functions for all standard responses
//! - [`writer`]  — `serialize` and `write_nonblocking`
pub mod builder;
pub mod types;
pub mod writer;