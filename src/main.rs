mod config;
mod event_loop;
mod handlers;
mod http;
mod router;
mod server;
mod utils;
mod cgi;
mod session;

use std::sync::atomic::{AtomicBool, Ordering};

use event_loop::{dispatcher, epoll::Epoll, registry::Registry};
use server::listener::bind_listeners;

/// Async-signal-safe shutdown flag setter.
/// Called by signal handlers (SIGTERM / SIGINT).
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Signal handler — sets the global shutdown flag.
fn handle_signal(_signum: libc::c_int) {
    // Writing to an AtomicBool with Relaxed ordering is async-signal-safe —
    // no malloc, no futex syscalls, just a plain memory store.
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let config_path = match args.get(1) {
        Some(p) => p.as_str(),
        None => {
            eprintln!("Usage: localhost <config-file>");
            std::process::exit(1);
        }
    };

    // ---- 0. Install signal handlers --------------------------------------

    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = handle_signal as *const () as usize;
    sa.sa_flags     = libc::SA_RESTART;

    unsafe {
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT,  &sa, std::ptr::null_mut());
    }

    // ---- 1. Parse and validate config --------------------------------------
    let configs = match config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ERROR] Configuration error: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("[INFO] Loaded {} server block(s):", configs.len());
    for (i, s) in configs.iter().enumerate() {
        eprintln!(
            "  [{}] {}:{:?}  routes: {}  body-limit: {} bytes",
            i + 1,
            s.host,
            s.ports,
            s.routes.len(),
            s.client_body_limit,
        );
    }

    // ---- 2. Create epoll instance ------------------------------------------
    let epoll = match Epoll::create() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[ERROR] Failed to create epoll: {e}");
            std::process::exit(1);
        }
    };

    // ---- 3. Bind all listening sockets -------------------------------------
    let mut registry = Registry::new();
    match bind_listeners(&configs, &epoll, &mut registry) {
        Ok(bound) => {
            eprintln!("[INFO] Bound {} listener(s):", bound.len());
            for (fd, addr) in &bound {
                eprintln!("  fd={fd}  addr={addr}");
            }
        }
        Err(e) => {
            eprintln!("[ERROR] Failed to bind listeners: {e}");
            std::process::exit(1);
        }
    }

    // ---- 4. Enter the event loop -------------------------------------------
    let exit_code = dispatcher::run(epoll, registry, configs, &SHUTDOWN_REQUESTED);
    eprintln!("[INFO] Server stopped (exit code {exit_code})");
    std::process::exit(exit_code);
}
