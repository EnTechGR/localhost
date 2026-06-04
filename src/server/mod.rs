//! Server-level abstractions: TCP listeners and per-connection state machines.
//!
//! - [`listener`]   — binds TCP sockets and implements `accept_one`.
//! - [`connection`] — `ConnectionState` and the `ConnectionPhase` state machine.
pub mod connection;
pub mod listener;