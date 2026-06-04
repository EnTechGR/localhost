/// TCP listener setup.
///
/// Binds one non-blocking `SO_REUSEADDR` listening socket per `(host, port)`
/// pair derived from the parsed `ServerConfig` list, then registers each
/// socket with both the epoll instance and the connection registry.
///
/// # Why one socket per (host, port)?
///
/// Multiple `ServerConfig` blocks may share the same `host:port` (virtual
/// hosting). We bind **one** socket per unique `(host, port)` pair and tag it
/// with the index of the **first** matching config. The dispatcher consults
/// the `Host` header later to select the right virtual server.
///
/// # Non-blocking requirement
///
/// The spec says "all I/O operations should be non-blocking". We set
/// `O_NONBLOCK` on every fd right after creation, before any data can arrive.
use std::ffi::CString;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;

use libc::{
    AF_INET, AF_INET6, SOCK_STREAM, SOL_SOCKET, SO_REUSEADDR, SO_REUSEPORT,
    F_SETFL, O_NONBLOCK, F_GETFL, IPPROTO_TCP, TCP_NODELAY,
};

use crate::config::types::ServerConfig;
use crate::event_loop::epoll::{Epoll, EPOLLIN};
use crate::event_loop::registry::Registry;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ListenerError {
    /// `socket(2)` call failed.
    SocketFailed { host: String, port: u16, errno: i32 },
    /// `setsockopt(2)` call failed.
    SetsockoptFailed { option: &'static str, errno: i32 },
    /// `bind(2)` call failed. Often `EADDRINUSE`.
    BindFailed { addr: String, errno: i32 },
    /// `listen(2)` call failed.
    ListenFailed { addr: String, errno: i32 },
    /// `fcntl` to set `O_NONBLOCK` failed.
    SetNonblockFailed { fd: RawFd, errno: i32 },
    /// The host string could not be resolved to an IP address.
    InvalidHost(String),
}

impl std::fmt::Display for ListenerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListenerError::SocketFailed { host, port, errno } =>
                write!(f, "socket() failed for {host}:{port}: errno {errno}"),
            ListenerError::SetsockoptFailed { option, errno } =>
                write!(f, "setsockopt({option}) failed: errno {errno}"),
            ListenerError::BindFailed { addr, errno } =>
                write!(f, "bind() failed for {addr}: errno {errno}"),
            ListenerError::ListenFailed { addr, errno } =>
                write!(f, "listen() failed for {addr}: errno {errno}"),
            ListenerError::SetNonblockFailed { fd, errno } =>
                write!(f, "fcntl(O_NONBLOCK) failed for fd {fd}: errno {errno}"),
            ListenerError::InvalidHost(h) =>
                write!(f, "cannot resolve host '{h}' to a bind address"),
        }
    }
}

impl std::error::Error for ListenerError {}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Bind all listening sockets required by `configs`.
///
/// Deduplicates `(host, port)` pairs so that multiple virtual servers sharing
/// a port only create one OS socket. Each socket is registered in both
/// `epoll` (for `EPOLLIN` events) and `registry` (tagged with the index of
/// the first `ServerConfig` that claimed the pair).
///
/// Returns the list of bound `(fd, addr)` pairs for logging purposes.
pub fn bind_listeners(
    configs:  &[ServerConfig],
    epoll:    &Epoll,
    registry: &mut Registry,
) -> Result<Vec<(RawFd, SocketAddr)>, ListenerError> {
    // Collect unique (host, port) → first server_id mappings.
    let mut seen: std::collections::HashMap<(String, u16), usize> =
        std::collections::HashMap::new();

    for (idx, server) in configs.iter().enumerate() {
        for &port in &server.ports {
            seen.entry((server.host.clone(), port)).or_insert(idx);
        }
    }

    let mut bound = Vec::new();

    for ((host, port), server_id) in &seen {
        let addr_str = format!("{host}:{port}");
        let sock_addr = parse_socket_addr(&addr_str, host, *port)?;

        let fd = create_tcp_socket(&sock_addr, host, *port)?;

        set_nonblocking(fd)?;
        set_reuseaddr(fd)?;
        set_reuseport(fd)?;
        set_tcp_nodelay(fd)?;

        bind(fd, &sock_addr)?;
        listen(fd, 128)?;

        // Register with epoll: EPOLLIN fires when accept() won't block.
        epoll
            .add(fd, EPOLLIN as u32, fd as u64)
            .map_err(|e| ListenerError::SetsockoptFailed {
                option: "epoll_add",
                errno:  extract_epoll_errno(&e),
            })?;

        registry.register_listener(fd, *server_id, *port);
        bound.push((fd, sock_addr));

        eprintln!(
            "[INFO] Listening on {addr_str} (server_id={server_id}, fd={fd})"
        );
    }

    Ok(bound)
}

// ---------------------------------------------------------------------------
// Socket primitives
// ---------------------------------------------------------------------------

/// Parse `"host:port"` into a `SocketAddr`.
fn parse_socket_addr(
    addr_str: &str,
    host:     &str,
    port:     u16,
) -> Result<SocketAddr, ListenerError> {
    addr_str.parse::<SocketAddr>().map_err(|_| {
        // Try prepending IPv6 brackets if it looks like an IPv6 address.
        let bracketed = format!("[{host}]:{port}");
        bracketed.parse::<SocketAddr>().unwrap_or_else(|_| {
            // Return a dummy to satisfy the type; the outer Err path is taken.
            "0.0.0.0:0".parse().unwrap()
        });
        ListenerError::InvalidHost(host.to_string())
    })
}

/// Create a `SOCK_STREAM` socket appropriate for `addr`.
fn create_tcp_socket(
    addr: &SocketAddr,
    host: &str,
    port: u16,
) -> Result<RawFd, ListenerError> {
    let family = if addr.is_ipv6() { AF_INET6 } else { AF_INET };
    let fd = unsafe { libc::socket(family, SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(ListenerError::SocketFailed {
            host:  host.to_string(),
            port,
            errno: errno(),
        });
    }
    Ok(fd)
}

/// Set `O_NONBLOCK` on `fd`.
fn set_nonblocking(fd: RawFd) -> Result<(), ListenerError> {
    let flags = unsafe { libc::fcntl(fd, F_GETFL, 0) };
    if flags < 0 {
        return Err(ListenerError::SetNonblockFailed { fd, errno: errno() });
    }
    let rc = unsafe { libc::fcntl(fd, F_SETFL, flags | O_NONBLOCK) };
    if rc < 0 {
        return Err(ListenerError::SetNonblockFailed { fd, errno: errno() });
    }
    Ok(())
}

/// Enable `SO_REUSEADDR` so the port can be rebound after a crash.
fn set_reuseaddr(fd: RawFd) -> Result<(), ListenerError> {
    let val: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            SOL_SOCKET,
            SO_REUSEADDR,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of_val(&val) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(ListenerError::SetsockoptFailed { option: "SO_REUSEADDR", errno: errno() });
    }
    Ok(())
}

/// Enable `SO_REUSEPORT` so multiple processes could share the port if needed.
fn set_reuseport(fd: RawFd) -> Result<(), ListenerError> {
    let val: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            SOL_SOCKET,
            SO_REUSEPORT,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of_val(&val) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(ListenerError::SetsockoptFailed { option: "SO_REUSEPORT", errno: errno() });
    }
    Ok(())
}

/// Disable Nagle's algorithm: we control buffering ourselves and want low
/// latency on small responses.
fn set_tcp_nodelay(fd: RawFd) -> Result<(), ListenerError> {
    let val: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            IPPROTO_TCP,
            TCP_NODELAY,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of_val(&val) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(ListenerError::SetsockoptFailed { option: "TCP_NODELAY", errno: errno() });
    }
    Ok(())
}

/// Call `bind(2)` on `fd` with `addr`.
fn bind(fd: RawFd, addr: &SocketAddr) -> Result<(), ListenerError> {
    let (sockaddr_ptr, sockaddr_len) = sockaddr_of(addr);
    let rc = unsafe { libc::bind(fd, sockaddr_ptr, sockaddr_len) };
    if rc < 0 {
        return Err(ListenerError::BindFailed {
            addr:  addr.to_string(),
            errno: errno(),
        });
    }
    Ok(())
}

/// Call `listen(2)` with the given backlog.
fn listen(fd: RawFd, backlog: libc::c_int) -> Result<(), ListenerError> {
    let rc = unsafe { libc::listen(fd, backlog) };
    if rc < 0 {
        return Err(ListenerError::ListenFailed {
            addr:  format!("fd={fd}"),
            errno: errno(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// accept() helper — used by the dispatcher
// ---------------------------------------------------------------------------

/// Outcome of a non-blocking `accept4` call.
pub enum AcceptResult {
    /// A new connection was accepted.
    Accepted { fd: RawFd, peer: SocketAddr },
    /// `accept4` returned `EAGAIN`/`EWOULDBLOCK` — no more pending connections.
    WouldBlock,
    /// A real error occurred.
    Error(i32),
}

/// Accept one pending connection from `listener_fd` without blocking.
///
/// Uses `accept4` with `SOCK_NONBLOCK | SOCK_CLOEXEC` so the new socket
/// inherits both flags atomically, avoiding a separate `fcntl` call.
pub fn accept_one(listener_fd: RawFd) -> AcceptResult {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

    let fd = unsafe {
        libc::accept4(
            listener_fd,
            &mut storage as *mut _ as *mut libc::sockaddr,
            &mut len,
            libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        )
    };

    if fd < 0 {
        let e = errno();
        if e == libc::EAGAIN || e == libc::EWOULDBLOCK {
            return AcceptResult::WouldBlock;
        }
        return AcceptResult::Error(e);
    }

    let peer = sockaddr_storage_to_socket_addr(&storage);
    AcceptResult::Accepted { fd, peer }
}

// ---------------------------------------------------------------------------
// sockaddr conversion helpers
// ---------------------------------------------------------------------------

/// Convert a `SocketAddr` into a raw `(ptr, len)` suitable for `bind`.
///
/// Returns a pointer into stack-allocated memory in the caller's frame — only
/// valid for the duration of the call site.
fn sockaddr_of(addr: &SocketAddr) -> (*const libc::sockaddr, libc::socklen_t) {
    match addr {
        SocketAddr::V4(v4) => {
            let sin = libc::sockaddr_in {
                sin_family: AF_INET as libc::sa_family_t,
                sin_port:   v4.port().to_be(),
                sin_addr:   libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: sin lives until end of function, caller uses ptr immediately.
            let ptr = Box::into_raw(Box::new(sin)) as *const libc::sockaddr;
            (ptr, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
        }
        SocketAddr::V6(v6) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family:   AF_INET6 as libc::sa_family_t,
                sin6_port:     v6.port().to_be(),
                sin6_addr:     libc::in6_addr { s6_addr: v6.ip().octets() },
                sin6_flowinfo: v6.flowinfo(),
                sin6_scope_id: v6.scope_id(),
            };
            let ptr = Box::into_raw(Box::new(sin6)) as *const libc::sockaddr;
            (ptr, std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
        }
    }
}

/// Convert the `sockaddr_storage` populated by `accept4` into a `SocketAddr`.
fn sockaddr_storage_to_socket_addr(storage: &libc::sockaddr_storage) -> SocketAddr {
    match storage.ss_family as libc::c_int {
        AF_INET => {
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip  = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            SocketAddr::from((ip, u16::from_be(sin.sin_port)))
        }
        AF_INET6 => {
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip   = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            SocketAddr::from((ip, u16::from_be(sin6.sin6_port)))
        }
        _ => "0.0.0.0:0".parse().unwrap(),
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

#[inline]
fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

fn extract_epoll_errno(e: &crate::event_loop::epoll::EpollError) -> i32 {
    // We only need an integer for the ListenerError; epoll errors are rare here.
    let _ = e;
    -1
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{RouteConfig, ServerConfig};
    use crate::event_loop::epoll::Epoll;
    use crate::event_loop::registry::Registry;

    fn test_server(host: &str, port: u16) -> ServerConfig {
        ServerConfig {
            host:   host.into(),
            ports:  vec![port],
            routes: vec![RouteConfig { path: "/".into(), root: Some("/tmp".into()), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn bind_single_listener() {
        let configs  = vec![test_server("127.0.0.1", 0)];
        // Port 0 lets the OS pick a free port.
        // We can't use port 0 with our current bind path because we store the
        // configured port — so instead we use a high ephemeral port.
        // This test just checks no panic / error on a basic bind.
        let configs = vec![test_server("127.0.0.1", 17890)];
        let epoll    = Epoll::create().unwrap();
        let mut reg  = Registry::new();
        let result   = bind_listeners(&configs, &epoll, &mut reg);
        // Clean up regardless.
        if let Ok(ref bound) = result {
            for &(fd, _) in bound {
                unsafe { libc::close(fd) };
            }
        }
        assert!(result.is_ok(), "bind failed: {:?}", result.err());
    }

    #[test]
    fn deduplicated_binding_for_virtual_hosts() {
        // Two servers on the same host:port should produce exactly ONE socket.
        let mut a = test_server("127.0.0.1", 17891);
        a.server_names = vec!["foo.example.com".into()];
        let mut b = test_server("127.0.0.1", 17891);
        b.server_names = vec!["bar.example.com".into()];

        let configs = vec![a, b];
        let epoll   = Epoll::create().unwrap();
        let mut reg = Registry::new();
        let result  = bind_listeners(&configs, &epoll, &mut reg);

        if let Ok(ref bound) = result {
            assert_eq!(bound.len(), 1, "expected exactly one socket for shared host:port");
            for &(fd, _) in bound {
                unsafe { libc::close(fd) };
            }
        }
        assert!(result.is_ok());
    }

    #[test]
    fn listener_registered_in_registry() {
        let configs = vec![test_server("127.0.0.1", 17892)];
        let epoll   = Epoll::create().unwrap();
        let mut reg = Registry::new();
        let bound   = bind_listeners(&configs, &epoll, &mut reg).unwrap();

        assert_eq!(reg.listener_count(), 1);
        let fd = bound[0].0;
        assert!(reg.is_listener(fd));

        unsafe { libc::close(fd) };
    }

    #[test]
    fn accept_one_would_block_on_idle_listener() {
        // Bind a real listener, then immediately call accept_one — no client
        // has connected so it must return WouldBlock.
        let fd = unsafe {
            libc::socket(AF_INET, SOCK_STREAM | libc::SOCK_CLOEXEC, 0)
        };
        assert!(fd >= 0);

        let val: libc::c_int = 1;
        unsafe { libc::setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &val as *const _ as *const _, 4) };

        let addr: SocketAddr = "127.0.0.1:17893".parse().unwrap();
        let (ptr, len) = sockaddr_of(&addr);
        unsafe { libc::bind(fd, ptr, len) };
        unsafe { libc::listen(fd, 8) };
        // set nonblocking
        let flags = unsafe { libc::fcntl(fd, F_GETFL, 0) };
        unsafe { libc::fcntl(fd, F_SETFL, flags | O_NONBLOCK) };

        assert!(matches!(accept_one(fd), AcceptResult::WouldBlock));
        unsafe { libc::close(fd) };
    }
}