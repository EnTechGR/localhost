mod config;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let config_path = match args.get(1) {
        Some(p) => p.as_str(),
        None => {
            eprintln!("Usage: localhost <config-file>");
            std::process::exit(1);
        }
    };

    let configs = match config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ERROR] Configuration error: {e}");
            std::process::exit(1);
        }
    };

    println!("[INFO] Loaded {} server block(s):", configs.len());
    for (i, s) in configs.iter().enumerate() {
        println!(
            "  [{}] {}:{:?}  routes: {}  body-limit: {} bytes",
            i + 1,
            s.host,
            s.ports,
            s.routes.len(),
            s.client_body_limit,
        );
    }

    // Event loop will be wired in here in a future step.
    todo!("event loop not yet implemented");
}