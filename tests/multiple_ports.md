# Multi-Port / Multi-Server Integration Test

Verifies that the server can listen on multiple ports and instantiate multiple servers at the same time.

---

## Prerequisites

- `nc` (netcat) installed
- A web browser or `curl` for testing HTTP endpoints
- A build binary ready (`cargo build --release` or just use `cargo run`)

---

## Scenario 1 — Single server, multiple ports

### Step 1 — Start the server with multi-port config

Open **Terminal 1**:

```bash
cargo run -- config/multi.conf &
```

Wait until you see startup logs confirming both sockets are bound:

```
[INFO] Bound 2 listener(s):
  fd=<N>  addr=127.0.0.1:8080
  fd=<M>  addr=127.0.0.1:8081
```

**What this proves:** A single `server { ... }` block with `port 8080 8081` creates **two independent TCP listeners**, not just one.

### Step 2 — Verify port 8080 responds

Open **Terminal 2**:

```bash
curl -s http://127.0.0.1:8080/ | head -5
```

**Expected:** A valid HTTP response (likely `200 OK` with directory listing or `index.html` content). If it fails, port 8080 is not bound.

### Step 3 — Verify port 8081 responds independently

In **Terminal 2**:

```bash
curl -s http://127.0.0.1:8081/ | head -5
```

**Expected:** A valid HTTP response from the second listener on port 8081. It should return similar content (same config), but proving a *separate* socket is serving it confirms multi-port binding works.

### Step 4 — Verify both ports handle concurrent connections

In **Terminal 2**, open two more terminals or background both calls:

```bash
# Terminal A
curl -s http://127.0.0.1:8080/ > /tmp/port_8080.txt &

# Terminal B
curl -s http://127.0.0.1:8081/ > /tmp/port_8081.txt &
```

Then compare and clean up:

```bash
diff <(cat /tmp/port_8080.txt) <(cat /tmp/port_8081.txt)  # should be identical
rm -f /tmp/port_808*.txt
```

**What this proves:** Both ports are independently accepting connections and serving the same root directory concurrently — no interference between listeners.

---

## Scenario 2 — Two separate servers, different host:port pairs

### Step 5 — Create a two-server config

Create `config/two-servers.conf`:

```
server {
    host        127.0.0.1
    port        8090
    server_name alpha.local
    
    error_page  404 www/errors/404.html
    route / {
        methods     GET HEAD
        root        www
        directory_listing   on
    }
}

server {
    host        127.0.0.1
    port        8091
    server_name beta.local
    
    error_page  404 www/errors/404.html
    route / {
        methods     GET HEAD
        root        www
        directory_listing   on
    }
}
```

### Step 6 — Start the two-server instance

Back in **Terminal 1** (kill previous server first):

```bash
pkill -f 'cargo run'       # kill previous server if still running
sleep 1                    # let ports free up (SO_REUSEADDR helps but timing matters)
cargo run -- config/two-servers.conf &
```

Wait until you see:

```
[INFO] Loaded 2 server block(s):
  [1] 127.0.0.1:[8090]  routes: 1  body-limit: ... bytes
  [2] 127.0.0.1:[8091]  routes: 1  body-limit: ... bytes

[INFO] Bound 2 listener(s):
  fd=<N>  addr=127.0.0.1:8090
  fd=<M>  addr=127.0.0.1:8091
```

**What this proves:** Two separate server blocks each create their own config + listener pair, both bound simultaneously without conflicts.

### Step 7 — Verify both servers respond independently

In **Terminal 2**:

```bash
# Alpha server on port 8090
curl -s http://127.0.0.1:8090/ | head -3

echo "---"

# Beta server on port 8091
curl -s http://127.0.0.1:8091/ | head -3
```

**Expected:** Both return valid HTTP responses (likely `200 OK` or similar). They should be identical (same root, same config) but served from independent sockets — proving both servers are live simultaneously.

---

## Step 8 — Verify port separation (no cross-connection interference)

In **Terminal 2**:

```bash
# Connect to each port and close immediately via raw TCP
echo "" | nc -q 1 127.0.0.1 8090 > /dev/null 2>&1 &
echo "" | nc -q 1 127.0.0.1 8091 > /dev/null 2>&1

# Wait a moment, then verify both ports are still listening (should NOT have been affected by each other)
sleep 0.5
curl -s -o /dev/null -w "port 8090: HTTP %{http_code}\n" http://127.0.0.1:8090/
curl -s -o /dev/null -w "port 8091: HTTP %{http_code}\n" http://127.0.0.1:8091/
```

**Expected:** Both return `HTTP 200` (or another valid code). Proves that opening/closing connections on one port does not affect the other — no socket sharing, no fd leaks between listeners.

---

## Step 9 — Verify Host header routing across servers (optional, if virtual hosts implemented)

In **Terminal 2**:

```bash
# Alpha server expects "alpha.local"
curl -s -H "Host: alpha.local" http://127.0.0.1:8090/ | head -3

# Beta server expects "beta.local"
curl -s -H "Host: beta.local" http://127.0.0.1:8091/ | head -3
```

**Expected:** If virtual host routing is implemented, each `server_name` routes to its corresponding config block. Without virtual hosts, both return identical content (same root).

---

## Step 10 — Clean up

Back in **Terminal 1**:

```bash
pkill -f 'cargo run'
```

Or press `Ctrl+C` if running in the foreground.

---

## What each step proves

| Step | Proves |
|------|--------|
| **Step 1** | Single server block with multiple ports binds ALL of them — not just the first, not port conflict errors |
| **Steps 2–3** | Each port independently serves HTTP requests and is reachable via standard clients |
| **Step 4** | Both ports handle concurrent connections without interfering with each other |
| **Step 6** | Two separate server blocks each create independent config + listener pairs simultaneously |
| **Step 7** | Multi-server instances operate as distinct virtual hosts on their respective ports at the same time |
| **Step 8** | Opening/closing connections on one port does not affect others — proper socket isolation, no fd leaks |
| **Step 9** | `Host:` header routing works correctly across multiple servers on the same host (if implemented) |

---

## Troubleshooting

| Issue | Fix |
|-------|-----|
| `address already in use` on bind | Wait 1–2 seconds for OS to release the port, then retry. Or kill lingering processes: `pkill -f 'cargo run'` |
| `curl: Connection refused` | Verify server startup logs show `Bound N listener(s)`. Ensure ports (8080/8081 or 8090/8091) are not blocked by firewall |
| Same response from both ports | This is expected if both servers share the same `root` config. The test verifies **both respond independently**, not different content |
| `pkill: command not found` (some platforms) | Use `lsof -ti :8090 8091 \| xargs kill -9` or `fuser -k 8090/tcp 8091/tcp` instead |
