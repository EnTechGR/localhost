//! Server-level abstractions: TCP listeners, per-connection state, and timeouts.
//!
//! - [`listener`]   — binds TCP sockets and implements `accept_one`.
//! - [`connection`] — `ConnectionState` and the `ConnectionPhase` state machine.
//! - [`timeout`]    — phase-aware timeout constants and sweep logic.
pub mod connection;
pub mod listener;
pub mod timeout;