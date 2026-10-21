#!/usr/bin/env python3
"""End-to-end check of webhook alerts against the real binary.

The Rust tests drive the delivery code with in-process receivers. This starts
the actual executable, a real receiver on a real socket, and checks what
operators will rely on:

  1. `--test-webhook` delivers a signed message and the documented Python
     verification snippet accepts it (and rejects a tampered body).
  2. The same URL without `allow_private_webhook_targets` is refused at startup
     with a message that names the setting and does not echo the URL or secret,
     and the receiver sees no connection.
  3. A cloud-metadata URL is refused the same way.
  4. With `--full`: the server really sends a `flush_stalled` alert from its
     background task, signed, and sends nothing containing the secret to stderr.

Usage: scripts/webhook-e2e.py [path-to-binary] [--full]
"""
import hashlib
import hmac
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

BIN = next((a for a in sys.argv[1:] if not a.startswith("--")), "target/debug/mallard-metrics")
FULL = "--full" in sys.argv
SECRET = "e2e-secret-0123456789abcdef"
TOKEN = "TOKEN-ABC123-DO-NOT-LOG"
FAILURES = []


def check(ok, what):
    print(("ok    " if ok else "FAIL  ") + what)
    if not ok:
        FAILURES.append(what)


def verify(secret: bytes, headers, body: bytes, tolerance: int = 300) -> bool:
    """The snippet from docs/src/alerts-and-reports.md, verbatim."""
    ts = headers["X-Mallard-Timestamp"]
    sig = headers["X-Mallard-Signature"]
    if abs(time.time() - int(ts)) > tolerance:
        return False
    expected = "v1=" + hmac.new(secret, ts.encode() + b"." + body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, sig)


class Receiver(http.server.BaseHTTPRequestHandler):
    received = []

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        Receiver.received.append((self.path, dict(self.headers), body))
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Receiver)
port = server.server_address[1]
threading.Thread(target=server.serve_forever, daemon=True).start()
URL = f"http://127.0.0.1:{port}/hook/{TOKEN}"


def env(**extra):
    e = {k: v for k, v in os.environ.items() if not k.startswith("MALLARD_")}
    e.update(
        MALLARD_ALERTS_WEBHOOK_URL=URL,
        MALLARD_ALERTS_WEBHOOK_SECRET=SECRET,
        NO_COLOR="1",
    )
    e.update(extra)
    return e


def run_test_webhook(**extra):
    return subprocess.run([BIN, "--test-webhook"], env=env(**extra), capture_output=True, text=True, timeout=60)


# 1. delivery + signature
Receiver.received.clear()
r = run_test_webhook(
    MALLARD_ALERTS_ALLOW_HTTP_WEBHOOK="true", MALLARD_ALERTS_ALLOW_PRIVATE_WEBHOOK_TARGETS="true"
)
check(r.returncode == 0, f"--test-webhook exits 0 (got {r.returncode}: {r.stderr.strip()})")
check(len(Receiver.received) == 1, "the receiver got exactly one request")
if Receiver.received:
    path, headers, body = Receiver.received[0]
    check(path == f"/hook/{TOKEN}", "path forwarded")
    check(verify(SECRET.encode(), headers, body), "signature verifies with the documented snippet")
    check(not verify(SECRET.encode(), headers, body + b" "), "a tampered body does not verify")
    check(not verify(b"wrong-secret-0123456789", headers, body), "a wrong secret does not verify")
    payload = json.loads(body)
    check(payload["kind"] == "test" and payload["text"] == payload["content"], "payload has kind, text and content")
    check(headers["X-Mallard-Delivery"] == payload["delivery_id"], "delivery id header matches the body")
check(TOKEN not in r.stdout + r.stderr and SECRET not in r.stdout + r.stderr, "no URL token or secret in the output")

# 2. loopback refused without the flag
Receiver.received.clear()
r = run_test_webhook(MALLARD_ALERTS_ALLOW_HTTP_WEBHOOK="true")
check(r.returncode == 1, "loopback target is refused (exit 1)")
check("allow_private_webhook_targets" in r.stderr, "the message names the setting")
check(TOKEN not in r.stderr and SECRET not in r.stderr, "the message echoes neither URL token nor secret")
check(not Receiver.received, "nothing reached the receiver")

# 2b. http without the flag
r = run_test_webhook(MALLARD_ALERTS_ALLOW_PRIVATE_WEBHOOK_TARGETS="true")
check(r.returncode == 1 and "allow_http_webhook" in r.stderr, "http:// without allow_http_webhook is refused")

# 3. metadata endpoint
r = run_test_webhook(
    MALLARD_ALERTS_WEBHOOK_URL=f"https://169.254.169.254/latest/meta-data/{TOKEN}",
)
check(r.returncode == 1 and "allow_private_webhook_targets" in r.stderr, "the metadata address is refused")
check(TOKEN not in r.stderr, "and not echoed")

# 4. the running server sends a real alert
if FULL:
    Receiver.received.clear()
    with tempfile.TemporaryDirectory() as data:
        srv = subprocess.Popen(
            [BIN],
            env=env(
                MALLARD_ALERTS_ALLOW_HTTP_WEBHOOK="true",
                MALLARD_ALERTS_ALLOW_PRIVATE_WEBHOOK_TARGETS="true",
                MALLARD_ALERTS_CHECK_INTERVAL_SECS="10",
                MALLARD_ALERTS_FLUSH_STALLED_SECS="60",
                MALLARD_FLUSH_INTERVAL="3600",  # the flush loop's first tick is immediate; see below
                MALLARD_HOST="127.0.0.1",
                MALLARD_PORT="18499",
                MALLARD_DATA_DIR=data,
                RUST_LOG="mallard_metrics=info",
            ),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            deadline = time.time() + 150
            while time.time() < deadline and not any(b"flush_stalled" in b for _, _, b in Receiver.received):
                time.sleep(2)
        finally:
            srv.terminate()
            out, err = srv.communicate(timeout=30)
            err = out + err  # tracing writes to stdout; either stream counts as 'the log'
        stalled = [(h, b) for _, h, b in Receiver.received if b"flush_stalled" in b]
        check(bool(stalled), "the server sent a flush_stalled alert on its own")
        if stalled:
            h, b = stalled[0]
            check(verify(SECRET.encode(), h, b), "its signature verifies")
            doc = json.loads(b)
            check(doc["kind"] == "alert" and doc["alert"]["id"] == "flush_stalled", "alert id and kind")
            check(doc["alert"]["severity"] == "critical" and "details" in doc["alert"], "severity and machine details")
        check(SECRET not in err and TOKEN not in err, "the server's log contains neither secret nor URL token")
        check("Webhook alerts enabled" in err, "startup log says alerts are enabled")
        check(srv.returncode == 0, f"clean exit on SIGTERM (code {srv.returncode})")

print()
print("FAILED" if FAILURES else "all checks passed")
sys.exit(1 if FAILURES else 0)
