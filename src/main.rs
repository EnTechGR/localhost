mod config;
mod event_loop;
mod handlers;
mod http;
mod router;
mod server;
mod utils;
mod cgi;
mod session;

use event_loop::{dispatcher, epoll::Epoll, registry::Registry};
use server::listener::bind_listeners;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let config_path = match args.get(1) {
        Some(p) => p.as_str(),
        None => {
            eprintln!("Usage: localhost <config-file>");
            std::process::exit(1);
        }
    };

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

    // ---- 4. Enter the event loop (never returns) ---------------------------
    dispatcher::run(epoll, registry, configs);
}