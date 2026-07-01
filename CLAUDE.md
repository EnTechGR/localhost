# Custom Rust HTTP & CGI Server (CLAUDE.md)

## Project Overview
A high-performance, completely single-threaded, non-blocking HTTP/1.1 web server written in Rust. It utilizes low-level OS event multiplexing (`epoll` or equivalent via `libc`) to handle multiple concurrent server instances, route requests, manage file uploads, handle sessions/cookies, and execute CGI scripts via process forking—mirroring NGINX behavior.

## Core Architectural Rules & Constraints
* **Single-Threaded / Single-Process:** The entire core server loop **must** run on a single process and a single thread. 
* **No Async Runtimes:** Do **NOT** use `tokio`, `async-std`, `nix`, or any crates that abstract server features. 
* **Low-Level I/O:** Use standard network primitives (`std::net::TcpListener`, `TcpStream`) switched to **non-blocking mode**. Use the `libc` crate directly for `epoll` (or platform equivalent like `kqueue`) registration.
* **Epoll Pattern:** All reads and writes must pass through the event loop. Call `epoll` or its equivalent exactly **once per client/server communication cycle**.
* **Memory & Stability:** The server must **never crash**, panic, or leak memory under any circumstances. Minimize `unsafe` use; do not abuse it.

## Build & Test Commands
* Build Release Binary: `cargo build --release`
* Check Types & Lints: `cargo clippy`
* Run Server: `cargo run -- <path_to_config_file>`
* Run Tests: `cargo test`

## Configuration File Requirements
The server parses a custom text configuration file supporting:
* `host` (server_address) and multiple `port` bindings per server.
* Default server selection for host:port if `server_name` doesn't match.
* Custom error page paths (for codes 400, 403, 404, 405, 413, 500).
* `client_max_body_size` enforcement for uploads.
* Route Definitions (No regex support needed):
  * Accepted HTTP methods (`GET`, `POST`, `DELETE`).
  * HTTP Redirections.
  * Directory root mapping (e.g., `/test` -> `/usr/Desktop`).
  * Default directory index files.
  * CGI script configuration per file extension (e.g., `.php`, `.py`).
  * Toggleable directory listing (on/off).

## HTTP & CGI Specifications
* **Protocol:** Strict compliance with `HTTP/1.1` (Chunked and unchunked transfer encoding, proper status codes, connection timeouts, cookies, and session handling).
* **CGI Execution:** Fork a new process to run the matching interpreter based on extension. Pass the target script file path as the first argument, stream the request body into stdin until `EOF`, set `PATH_INFO`, and correctly execute within the expected working directory.

## Quality Assurance Boundaries
* **Availability:** Must maintain $\ge 99.5\%$ uptime/success rate under `siege -b [IP]:[PORT]` stress testing.
* **Timeouts:** Aggressively timeout slow or hanging client requests to prevent resource starvation.
* **Error Handling:** Gracefully handle malformed requests, large payload violations (413), and missing files (404) using custom error pages without triggering a Rust `panic!`.

---

## Audit & Defense Verification Checklist
When writing code or modifications, verify that the following defense requirements are satisfied and document where they are implemented so the developer can explain them to auditors.

### 1. Functional & I/O Multiplexing Verification
* **Multiplexing Engine:** Ensure the core loop uses a single `epoll` / `kqueue` instance to handle all active server sockets and client connections.
* **Single Epoll Iteration:** Verify that for each event loop iteration, only **one** read or write operation is executed per client to avoid blocking the single thread.
* **Robust Return Checking:** Every single low-level I/O return value (`libc::read`, `libc::write`, `accept`) must be strictly checked. If an unrecoverable error occurs on a socket, immediately remove the client from the multiplexing pool and close the descriptor.
* **Strict Non-Blocking:** Confirm that no I/O operation happens outside of the `epoll` cycle.

### 2. Configuration Fault Tolerance
* **Port Conflict Isolation:** If a configuration file attempts to bind duplicate ports on identical host addresses, catch the error explicitly during setup and exit gracefully.
* **Partial Configuration Resilience:** If multiple independent servers are defined in a single configuration file, an error/invalid layout in *one* server blocks that specific block from initializing but must **not** crash or prevent the remaining valid servers from binding and running on their respective ports.
* **Host Header Routing:** Verify that when multiple servers share a port but use different `server_name` entries, incoming HTTP requests are directed based strictly on the parsed `Host` header.

### 3. HTTP Methods, Headers & Browser Behavior
* **Payload Verification:** Ensure file uploads do not corrupt binary buffers (validate md5/checksum hash parity).
* **Cookie/Session Continuity:** Maintain persistent cookie tokens across browser refreshes.
* **Edge Routing:** Explicitly test and return appropriate status codes for directory listings, 3xx redirections, chunked payloads passing into the CGI subsystem, and intentionally malformed HTTP requests.

### 4. Stress Tests & Leak Validation
* Run profiling checks regularly. The single thread must handle high-throughput parallel bombardment via `siege -b` without generating memory leaks, hanging sockets, or drops in availability below $99.5\%$.
