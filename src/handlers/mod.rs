//! Per-method and per-concern request handlers.
//!
//! - [`static_file`] — GET/HEAD static file serving
//! - [`upload`]      — POST file upload (multipart + raw body)
//! - [`delete`]      — DELETE resource removal
//! - [`directory`]   — HTML directory listing
//! - [`redirect`]    — redirect response construction
//! - [`error`]       — error page rendering with custom page support
pub mod delete;
pub mod directory;
pub mod error;
pub mod redirect;
pub mod static_file;
pub mod upload;