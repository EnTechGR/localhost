# Live Timeout Integration Test

Verifies that the server correctly sends `408 Request Timeout` for incomplete requests and does **not** time out valid requests.

---

## Prerequisites

- `nc` (netcat) installed
- A build binary: `cargo build --release` or `cargo run`

---

## Step 1 — Start the server with a short timeout

Open **Terminal 1**:

```bash
TIMEOUT_HEADERS_SECS=1 cargo run -- config/default.conf &
```

Wait until you see startup logs confirming the server is bound and listening.

**What this does:** `TIMEOUT_HEADERS_SECS=1` overrides the default 30-second header timeout to just **1 second**, making the test fast. The server runs with a real epoll loop and actual TCP sockets.

---

## Step 2 — Send incomplete headers and capture the 408

Open **Terminal 2**:

```bash
# Send partial HTTP headers (never completes the \r\n\r\n)
printf 'GET / HTTP/1.1\r\nHost: localhost\r\n' | nc -q 3 127.0.0.1 8080
```

Wait ~2 seconds, then `nc` will return after timing out on its own. You should see the full response text printed by `nc`.

**Expected output:**

```
HTTP/1.1 408 Request Timeout
Content-Type: text/html; charset=utf-8
Content-Length: 101
Connection: close

<html><head><title>408 Request Timeout</title></head><body><h1>408 Request Timeout</h1></body></html>
```

**Checklist:**

- [ ] `HTTP/1.1 408` appears on the first line
- [ ] `Content-Length: 101` matches the body length
- [ ] Response body contains `<h1>408 Request Timeout</h1>`
- [ ] Total bytes received ≈ 213 (header + `\r\n\r\n` + body)

---

## Step 3 — Verify the server logged the timeout

Go back to **Terminal 1** and look for:

```
[INFO] Timeout (408) on fd=<some-number>
```

This confirms the server's `check_timeouts()` sweep fired and chose the correct status code.

---

## Step 4 — Test a complete request (should NOT timeout)

Open **Terminal 2** again:

```bash
# Send a complete request (headers + \r\n\r\n)
printf 'GET /\r\nHost: localhost\r\n\r\n' | nc -q 1 127.0.0.1 8080
```

**Expected:** You should get back the server's normal response (likely a `404 Not Found` or similar). This confirms only *incomplete* requests time out — valid requests complete normally.

---

## Step 5 — Clean up

Back in **Terminal 1**:

```bash
# Find and kill the server process
pkill -f 'cargo run'
```

Or press `Ctrl+C` if it's running in the foreground.

---

## What each step proves

| Step | Proves |
|------|--------|
| **Step 2** | Server sends `408 Request Timeout` on actual TCP when headers are incomplete |
| **Step 3** | The server-side `check_timeouts()` sweep is actually running and classifying correctly |
| **Step 4** | Valid requests complete normally — timeout doesn't false-positive on working connections |
