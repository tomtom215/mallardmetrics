import socket, subprocess, time, os, signal, sys, json, tempfile
bin_, work = sys.argv[1], tempfile.mkdtemp(dir=sys.argv[2])
env = dict(os.environ, MALLARD_HOST="127.0.0.1", MALLARD_PORT="18377", MALLARD_DATA_DIR=work+"/data",
           MALLARD_FLUSH_COUNT="100000", MALLARD_FLUSH_INTERVAL="3600", MALLARD_SHUTDOWN_TIMEOUT="10")
p = subprocess.Popen([bin_], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
for _ in range(100):
    try: socket.create_connection(("127.0.0.1",18377),0.2).close(); break
    except OSError: time.sleep(0.1)
body = json.dumps({"d":"example.com","n":"pageview","u":"https://example.com/late","r":""}).encode()
ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120 Safari/537.36"
head = (f"POST /api/event HTTP/1.1\r\nHost: x\r\nUser-Agent: {ua}\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n").encode()
c = socket.create_connection(("127.0.0.1",18377))
c.sendall(head + body[:10])          # request is now in flight, body incomplete
time.sleep(0.5)
p.send_signal(signal.SIGTERM)         # shutdown begins while it is in flight
time.sleep(1.0)
c.sendall(body[10:])                  # the handler accepts the event AFTER the signal
resp = c.recv(4096).decode(errors="replace").split("\r\n")[0]
out, _ = p.communicate(timeout=30)
print("response:", resp)
print("exit code:", p.returncode)
print("\n".join(l for l in out.splitlines() if any(k in l for k in ("Flushed","flush","Shutdown","drain","SIGTERM"))))
