/// Connection and listener registry.
///
/// Maintains two maps keyed by raw file descriptor:
///
/// - **Listeners** – server sockets waiting to `accept()` new connections.
///   Each is tagged with a `server_id` (index into `configs`) so the
///   dispatcher knows which `ServerConfig` owns an accepted connection.
///
/// - **Connections** – accepted client sockets, each with a full
///   `ConnectionState`.
///
/// # Design
///
/// Using `HashMap<RawFd, _>` gives O(1) lookup when epoll fires an event.
/// The token stored in `epoll_data.u64` is always the fd cast to `u64`, so
/// dispatch is: `event.u64 as RawFd` → registry lookup → handle.
///
/// Listener fds and connection fds are kept in separate maps so the
/// dispatcher can distinguish them with a single `contains_key` test rather
/// than a match on a tagged enum.
use std::collections::HashMap;
use std::os::unix::io::RawFd;

use crate::server::connection::ConnectionState;

// ---------------------------------------------------------------------------
// ListenerEntry
// ---------------------------------------------------------------------------

/// Metadata stored for each listening socket.
#[derive(Debug, Clone)]
pub struct ListenerEntry {
    /// Index into the `Vec<ServerConfig>` passed to the event loop.
    /// Every connection accepted on this socket inherits this `server_id`.
    pub server_id: usize,

    /// The port this listener is bound to (used for logging).
    pub port: u16,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct Registry {
    listeners:   HashMap<RawFd, ListenerEntry>,
    connections: HashMap<RawFd, ConnectionState>,
}

impl Registry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Registry {
            listeners:   HashMap::new(),
            connections: HashMap::new(),
        }
    }

    // ------------------------------------------------------------------ //
    // Listener management
    // ------------------------------------------------------------------ //

    /// Register a listening socket.
    ///
    /// Panics in debug builds if the fd is already registered (programming
    /// error: double-register the same fd).
    pub fn register_listener(&mut self, fd: RawFd, server_id: usize, port: u16) {
        debug_assert!(
            !self.listeners.contains_key(&fd) && !self.connections.contains_key(&fd),
            "fd {fd} already registered"
        );
        self.listeners.insert(fd, ListenerEntry { server_id, port });
    }

    /// Returns `true` if `fd` is a registered listener.
    #[inline]
    pub fn is_listener(&self, fd: RawFd) -> bool {
        self.listeners.contains_key(&fd)
    }

    /// Retrieve the listener entry for `fd`, if it exists.
    pub fn listener(&self, fd: RawFd) -> Option<&ListenerEntry> {
        self.listeners.get(&fd)
    }

    /// Iterate over all listener fds and their entries.
    pub fn listeners(&self) -> impl Iterator<Item = (RawFd, &ListenerEntry)> {
        self.listeners.iter().map(|(&fd, e)| (fd, e))
    }

    /// Remove a listener entry. Symmetric counterpart to `register_listener`,
    /// reserved for a graceful-shutdown path that is not yet wired.
    #[allow(dead_code)]
    pub fn remove_listener(&mut self, fd: RawFd) {
        self.listeners.remove(&fd);
    }

    // ------------------------------------------------------------------ //
    // Connection management
    // ------------------------------------------------------------------ //

    /// Register a newly accepted connection.
    ///
    /// Panics in debug builds if the fd is already registered.
    pub fn register_connection(&mut self, fd: RawFd, state: ConnectionState) {
        debug_assert!(
            !self.listeners.contains_key(&fd) && !self.connections.contains_key(&fd),
            "fd {fd} already registered"
        );
        self.connections.insert(fd, state);
    }

    /// Borrow the connection state for `fd` mutably.
    pub fn get_connection_mut(&mut self, fd: RawFd) -> Option<&mut ConnectionState> {
        self.connections.get_mut(&fd)
    }

    /// Borrow the connection state for `fd` immutably.
    pub fn get_connection(&self, fd: RawFd) -> Option<&ConnectionState> {
        self.connections.get(&fd)
    }

    /// Remove and return the connection state for `fd`.
    ///
    /// Called by the dispatcher just before closing the fd.
    pub fn remove(&mut self, fd: RawFd) -> Option<ConnectionState> {
        self.connections.remove(&fd)
    }

    /// Returns a snapshot of all connection fds.
    ///
    /// Allocates a `Vec`; intended for the timeout sweep that iterates all
    /// connections once per epoll tick. The allocation is acceptable because
    /// the sweep is infrequent relative to per-event dispatch.
    pub fn all_connection_fds(&self) -> Vec<RawFd> {
        self.connections.keys().copied().collect()
    }

    // ------------------------------------------------------------------ //
    // Metrics / diagnostics
    // ------------------------------------------------------------------ //

    /// Number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Number of registered listeners.
    pub fn listener_count(&self) -> usize {
        self.listeners.len()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::connection::ConnectionState;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn conn(fd: RawFd) -> ConnectionState {
        ConnectionState::new(fd, 0, 9000, addr(9000))
    }

    // ---- listeners ---------------------------------------------------------

    #[test]
    fn register_and_lookup_listener() {
        let mut reg = Registry::new();
        reg.register_listener(10, 0, 8080);
        assert!(reg.is_listener(10));
        assert_eq!(reg.listener(10).unwrap().server_id, 0);
        assert_eq!(reg.listener(10).unwrap().port,      8080);
    }

    #[test]
    fn unknown_fd_is_not_listener() {
        let reg = Registry::new();
        assert!(!reg.is_listener(99));
        assert!(reg.listener(99).is_none());
    }

    #[test]
    fn remove_listener() {
        let mut reg = Registry::new();
        reg.register_listener(10, 0, 8080);
        reg.remove_listener(10);
        assert!(!reg.is_listener(10));
    }

    #[test]
    fn listener_count() {
        let mut reg = Registry::new();
        assert_eq!(reg.listener_count(), 0);
        reg.register_listener(10, 0, 8080);
        reg.register_listener(11, 1, 9090);
        assert_eq!(reg.listener_count(), 2);
    }

    // ---- connections -------------------------------------------------------

    #[test]
    fn register_and_lookup_connection() {
        let mut reg = Registry::new();
        reg.register_connection(20, conn(20));
        assert!(reg.get_connection(20).is_some());
        assert_eq!(reg.get_connection(20).unwrap().fd, 20);
    }

    #[test]
    fn get_connection_mut_allows_mutation() {
        let mut reg = Registry::new();
        reg.register_connection(20, conn(20));
        reg.get_connection_mut(20).unwrap().keep_alive = true;
        assert!(reg.get_connection(20).unwrap().keep_alive);
    }

    #[test]
    fn remove_connection_returns_state() {
        let mut reg = Registry::new();
        reg.register_connection(20, conn(20));
        let state = reg.remove(20).expect("should return state");
        assert_eq!(state.fd, 20);
        assert!(reg.get_connection(20).is_none());
    }

    #[test]
    fn remove_unknown_fd_returns_none() {
        let mut reg = Registry::new();
        assert!(reg.remove(99).is_none());
    }

    #[test]
    fn all_connection_fds_snapshot() {
        let mut reg = Registry::new();
        reg.register_connection(20, conn(20));
        reg.register_connection(21, conn(21));
        reg.register_connection(22, conn(22));
        let mut fds = reg.all_connection_fds();
        fds.sort();
        assert_eq!(fds, vec![20, 21, 22]);
    }

    #[test]
    fn connection_count() {
        let mut reg = Registry::new();
        assert_eq!(reg.connection_count(), 0);
        reg.register_connection(20, conn(20));
        assert_eq!(reg.connection_count(), 1);
        reg.remove(20);
        assert_eq!(reg.connection_count(), 0);
    }

    #[test]
    fn listeners_iter() {
        let mut reg = Registry::new();
        reg.register_listener(10, 0, 8080);
        reg.register_listener(11, 1, 9090);
        let count = reg.listeners().count();
        assert_eq!(count, 2);
    }

    // ---- separation of listener and connection maps -----------------------

    #[test]
    fn listener_fd_not_in_connections() {
        let mut reg = Registry::new();
        reg.register_listener(10, 0, 8080);
        // The same fd should NOT appear as a connection.
        assert!(reg.get_connection(10).is_none());
    }
}