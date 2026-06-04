//! Request routing: virtual-host selection and per-route dispatch.
//!
//! - [`matcher`] — `select_server` and `match_route`
//! - [`handler`] — `dispatch` orchestrates method check → redirect → CGI → static
pub mod handler;
pub mod matcher;