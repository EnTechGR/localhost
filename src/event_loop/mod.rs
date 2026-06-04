//! Non-blocking I/O event loop built on Linux `epoll`.
//!
//! # Module layout
//!
//! - [`epoll`]      — thin safe wrapper around the `epoll_*` syscalls.
//! - [`registry`]   — maps raw file descriptors to listener / connection state.
//! - [`dispatcher`] — the main `run()` loop and per-event handlers.
//!
//! # Entry point
//!
//! ```rust,no_run
//! use crate::event_loop::{epoll::Epoll, registry::Registry, dispatcher};
//! use crate::config::types::ServerConfig;
//!
//! let epoll    = Epoll::create().expect("epoll_create1 failed");
//! let registry = Registry::new();
//! let configs: Vec<ServerConfig> = vec![/* ... */];
//!
//! // bind_listeners populates registry and epoll before we hand off.
//! dispatcher::run(epoll, registry, configs); // → !
//! ```
pub mod dispatcher;
pub mod epoll;
pub mod registry;