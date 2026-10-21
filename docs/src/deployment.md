# Deployment

Mallard Metrics is one process plus a data directory. Pick a guide:

| You have | Use |
|---|---|
| A generic Linux VPS and a Cloudflare-managed domain | [VPS Deployment Guide](deploy-vps.md): Caddy, TLS, encrypted volume, backups, one script |
| Fly.io | [Fly.io Deployment](deploy-flyio.md) (not verified end to end; read its caveats) |
| An existing Docker host with its own proxy | this page: Docker and Compose |
| No Docker | this page: systemd |

None of the commands on this page that need Docker or a cloud account were run
for this book (the build machine has no Docker daemon). The binary's behaviour
(startup checks, health, extension install, proxy handling) was verified; image
and compose commands were checked against the files in the repository only.

## What the image is

- **glibc**, not musl: a release binary (or a build from the repository
  `Dockerfile`) on distroless `gcr.io/distroless/cc-debian12:nonroot`: no shell, no
  package manager, UID/GID **65532**, `/data` as the volume and working
  directory, port 8000.
- **There is no musl or Alpine build and no `FROM scratch` image.** The
  `behavioral` DuckDB extension is not published for musl (the repository answers
  `404` for `linux_amd64_musl` and `linux_arm64_musl`), so a musl binary would
  serve everything except funnels, retention, sessions, sequences and flow, which
  would answer `503` while the image passed its health check.
- Platforms: `linux/amd64` and `linux/arm64`.
- The image sets `HOME=/data`, `MALLARD_DATA_DIR=/data` and
  `MALLARD_EXTENSION_DIR=/data/extensions`, so the extension is installed onto the
  volume even with a read-only root filesystem.
- `HEALTHCHECK` runs `/mallard-metrics --healthcheck` (exec form, because there is no
  shell), every 30 s with a 20 s start period.
- Images are `ghcr.io/tomtom215/mallardmetrics`. Tags are the semantic version
  without `v` (`0.2.0`), `0.2`, `0`, and the commit SHA. **Pin a version** rather than
  following a moving tag.

## Production checklist

- [ ] `MALLARD_SECRET`: a random value of **16+ characters** (`openssl rand -hex 32`),
      kept constant. Shorter values stop startup.
- [ ] `MALLARD_ADMIN_PASSWORD`: 12+ characters. Without it, the first visitor to reach
      `/api/auth/setup` can claim the instance.
- [ ] A TLS-terminating reverse proxy, then `MALLARD_SECURE_COOKIES=true` and
      `MALLARD_DASHBOARD_ORIGIN=https://analytics.example.com`.
- [ ] `MALLARD_TRUST_PROXY_HEADERS=true` and `MALLARD_TRUSTED_PROXY_HOPS` equal to the
      number of proxies you own ([below](#behind-a-reverse-proxy)).
- [ ] `MALLARD_METRICS_TOKEN` if `/metrics` is reachable beyond Prometheus.
- [ ] A persistent volume for `/data`, writable by UID 65532.
- [ ] `MALLARD_SITE_IDS` (or `MALLARD_RATE_LIMIT_PER_IP`): otherwise anyone can create
      sites and write events (the server warns about this).
- [ ] `MALLARD_RETENTION_DAYS` to match your retention policy.
- [ ] `MALLARD_DUCKDB_MEMORY_LIMIT` about 60% of the container limit.
- [ ] A stop grace period of at least 45 s (`stop_grace_period`, `docker run
      --stop-timeout 45`, systemd `TimeoutStopSec=45`): the shutdown flush can take
      up to the shutdown timeout.
- [ ] The behavioral extension is seeded if the container has no outbound route.
- [ ] [Backups](backup-restore.md) scheduled, and a restore tested.
- [ ] [Monitoring](monitoring.md): scrape `/metrics`, alert on `/health/detailed`.
- [ ] Use `/health/ready` for readiness.

EU deployments additionally: see the checklist on
[Privacy and GDPR](privacy-gdpr.md#checklist).

---

## Docker

```bash
docker run -d \
  --name mallard-metrics \
  --restart unless-stopped \
  --stop-timeout 45 \
  -p 127.0.0.1:8000:8000 \
  -v mallard-data:/data \
  -e MALLARD_SECRET="$(openssl rand -hex 32)" \
  -e MALLARD_ADMIN_PASSWORD='choose-a-long-password' \
  -e MALLARD_SECURE_COOKIES=true \
  -e MALLARD_METRICS_TOKEN="$(openssl rand -hex 32)" \
  ghcr.io/tomtom215/mallardmetrics:0.2.0
```

**Keep the generated secret**: store it where you keep other credentials. Losing it
re-identifies every visitor. With `-p 127.0.0.1:8000:8000` the port is reachable only
from the host; put a TLS proxy in front.

The first start downloads the `behavioral` extension (a few MB) onto the volume, so
the container needs outbound access once; it starts either way.

With a config file:

```bash
docker run -d --name mallard-metrics --stop-timeout 45 \
  -v mallard-data:/data \
  -v /etc/mallard-metrics/config.toml:/config.toml:ro \
  -e MALLARD_SECRET=... -e MALLARD_ADMIN_PASSWORD=... \
  ghcr.io/tomtom215/mallardmetrics:0.2.0 /config.toml
```

## Docker Compose

The repository's `docker-compose.yml` builds from the Dockerfile and is hardened
(read-only root, no capabilities, `no-new-privileges`, memory and CPU limits, a
45 s stop grace period, the self-probing healthcheck). To use it:

```bash
echo "MALLARD_SECRET=$(openssl rand -hex 32)" > .env
echo "MALLARD_ADMIN_PASSWORD=choose-a-long-password" >> .env
docker compose up -d --build        # first build compiles DuckDB: several minutes
docker compose logs -f
```

It binds `127.0.0.1:8000` by default (`MALLARD_BIND`, `MALLARD_HOST_PORT`) and
reads `MALLARD_SECURE_COOKIES`, `MALLARD_METRICS_TOKEN`,
`MALLARD_DASHBOARD_ORIGIN`, `MALLARD_TRUST_PROXY_HEADERS` (default `false`),
`MALLARD_DUCKDB_MEMORY_LIMIT` (default 640 MB) and `MALLARD_DUCKDB_THREADS`
(default 2) from the environment. Note the compose file does not pass
`MALLARD_TRUSTED_PROXY_HOPS`; the default of 1 applies, which is right for one
proxy. For an internet-facing host use `deploy/docker-compose.production.yml`, see the
[VPS guide](deploy-vps.md).

To run a released image instead of building, replace `build: .` with the `image:`
line pointing at the tag you pinned.

---

## Behind a reverse proxy

Mallard binds `0.0.0.0:8000` by default; set `MALLARD_HOST=127.0.0.1` when the proxy
is on the same host. The client address feeds visitor IDs, GeoIP, the rate limiters
and the login lockout, so get this right:

1. Turn on `MALLARD_TRUST_PROXY_HEADERS=true` **only** when every request reaches
   Mallard through your proxy. On a directly reachable server the header is
   attacker-controlled.
2. Set `MALLARD_TRUSTED_PROXY_HOPS` to the number of proxies **you** own. Each proxy
   appends the address it received the connection from to `X-Forwarded-For`; Mallard
   takes that many entries counting **from the right**. Entries on the left were
   written by someone you do not control.

| Topology | `TRUSTED_PROXY_HOPS` |
|---|---|
| Client → Caddy → Mallard | 1 |
| Client → nginx → Mallard | 1 |
| Client → CDN → nginx (or Caddy) → Mallard | 2 |

Without step 1 every visitor is the proxy (and five wrong passwords from anyone lock
the admin out); with too small a hop count traffic is attributed to a proxy; with too
large a count a client chooses its own address.

### nginx

```nginx
server {
    listen 443 ssl;
    server_name analytics.example.com;
    ssl_certificate     /etc/ssl/certs/analytics.example.com.crt;
    ssl_certificate_key /etc/ssl/private/analytics.example.com.key;

    location / {
        proxy_pass http://127.0.0.1:8000;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

`$proxy_add_x_forwarded_for` appends the connecting address to whatever arrived,
which is exactly what the hop rule expects, so with nginx as the only proxy
`MALLARD_TRUSTED_PROXY_HOPS=1`. Setting `X-Forwarded-For $remote_addr;` instead is
also safe for one proxy (a single entry). With a CDN in front of nginx, keep the
appended form and use `2`. Confirm with a spoofed header:
`curl -H 'X-Forwarded-For: 1.2.3.4' https://analytics.example.com/...` must not
change which address Mallard attributes the request to. (Verified against a running
server with hops 1: a forged leftmost entry is ignored.)

### Caddy

```caddy
analytics.example.com {
    reverse_proxy 127.0.0.1:8000
}
```

With no `trusted_proxies` configured Caddy discards a client-sent
`X-Forwarded-For` and passes one entry, the address it accepted the connection from;
use `MALLARD_TRUSTED_PROXY_HOPS=1`. Do not add `trusted_proxies` unless a CDN really
sits in front. The bundled `deploy/Caddyfile` is this setup plus security headers and
a log format that records no visitor data.

### CDN in front (Cloudflare and similar)

Two correct ways, never mix them:

- Make the **proxy** write the CDN's client header into `X-Forwarded-For` (for Caddy:
  `header_up X-Forwarded-For {header.CF-Connecting-IP}`) and keep
  `TRUSTED_PROXY_HOPS=1`, and firewall the origin to the CDN's published ranges so
  nobody can bypass the CDN and forge the header.
- Or keep the CDN's chain and count it: with CDN → nginx → Mallard, nginx appends the
  CDN's address, the CDN appended the client's, so `TRUSTED_PROXY_HOPS=2`.

If you cannot say how many proxies append, do not enable trust: a wrong count is
silently wrong.

### After the proxy

```bash
MALLARD_SECURE_COOKIES=true
MALLARD_DASHBOARD_ORIGIN=https://analytics.example.com   # enables the CSRF origin check
```

---

## Health and readiness

| Endpoint | Purpose |
|---|---|
| `GET /health` | Liveness. |
| `GET /health/ready` | Readiness: queries DuckDB; `503` if not ready. |
| `GET /health/detailed` | Component status; `503` and `"degraded"` when the cold tier is unreadable. |

```yaml
livenessProbe:
  httpGet: { path: /health, port: 8000 }
  initialDelaySeconds: 5
  periodSeconds: 10
readinessProbe:
  httpGet: { path: /health/ready, port: 8000 }
  initialDelaySeconds: 10
  periodSeconds: 15
  failureThreshold: 3
```

The image has no shell, `curl` or `wget`, so Docker uses the binary itself:
`mallard-metrics --healthcheck` requests `/health/ready` on the loopback address and
exits non-zero if it does not answer 200. It reads **environment variables only**, so
a `data_dir` or `port` set in a TOML file is not seen by it (the image's `MALLARD_*`
variables are). Kubernetes ignores `HEALTHCHECK`; use the HTTP probes.

The server binds its port only after startup work, including an extension download
attempt, so allow time on the first start (the image allows a 20 s start period).

## Seeding the behavioral extension on an isolated network

If the container has no outbound route (the production compose layout does this on
purpose), run once:

```bash
docker run --rm --user 65532:65532 -v mallard-data:/data \
  ghcr.io/tomtom215/mallardmetrics:0.2.0 --install-extension
```

Details and the bare-metal and air-gapped variants:
[Troubleshooting](troubleshooting.md#running-the-extension-install-step-offline).

---

## Build from source

```bash
git clone https://github.com/tomtom215/mallardmetrics.git
cd mallardmetrics
cargo build --release          # Rust 1.98.0, pinned by rust-toolchain.toml
./target/release/mallard-metrics mallard-metrics.toml.example
```

DuckDB is compiled from source (the `bundled` feature), which takes several minutes
and a lot of disk (several GB in debug builds). Build for a `*-unknown-linux-gnu` or
Apple target. `scripts/check-binary-abi.sh <binary> <glibc> <libstdc++>` verifies a
binary does not need a newer glibc than the one it will run on (the image uses
Debian 12: glibc 2.36). Do not build a musl target: the behavioral extension cannot
load there.

## GeoIP

1. Create a free MaxMind account and download `GeoLite2-City.mmdb`.
2. Put it on the data volume and point at it:

```bash
docker run ... -v /path/GeoLite2-City.mmdb:/data/GeoLite2-City.mmdb:ro \
  -e MALLARD_GEOIP_DB=/data/GeoLite2-City.mmdb ...
```

A missing or unreadable file is not an error: geography is stored as `NULL` and
`/health/detailed` shows `geoip_loaded: false`. The database is read at startup, so
restart after replacing it. `MALLARD_GEOIP_PRECISION` limits what is stored.

## Privacy mode

`MALLARD_GDPR_MODE=true` bundles the privacy flags but also turns on
`round_timestamps`, which disables session, funnel, sequence, flow and entry/exit
reports. Read what it changes and what it costs on
[Privacy and GDPR](privacy-gdpr.md#gdpr-mode) before enabling it.

---

## Graceful shutdown

On `SIGTERM` (docker stop, systemd stop) or `SIGINT`:

1. The server stops accepting connections and drains in-flight requests, for at most
   `shutdown_timeout_secs` (default 30).
2. The event buffer is closed (a late request gets `503`) and flushed to Parquet,
   for at most `shutdown_timeout_secs` again.
3. The process exits.

Typically this takes a fraction of a second; the worst case is twice the timeout, so
give the platform at least `shutdown_timeout_secs + a margin` (45 s with the default
works for a normal flush; use 70 s or more if you want the full worst case).
Anything the platform's `SIGKILL` cuts off, including the unflushed buffer, is lost.

## systemd

`deploy/systemd/mallard-metrics.service` is a hardened unit (dynamic user, state
directory, sandboxing, `TimeoutStopSec=45`). Install a **glibc** release binary:

```bash
sudo install -m 0755 mallard-metrics /usr/local/bin/mallard-metrics
sudo install -d -m 0700 /etc/mallard-metrics
sudo install -m 0600 /dev/null /etc/mallard-metrics/mallard.env   # then edit
sudo install -m 0644 deploy/systemd/mallard-metrics.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now mallard-metrics
```

`mallard.env` holds `VAR=value` lines (no quotes, no `export`): `MALLARD_SECRET`,
`MALLARD_ADMIN_PASSWORD`, `MALLARD_METRICS_TOKEN`, `MALLARD_SECURE_COOKIES=true`,
`MALLARD_DASHBOARD_ORIGIN`, and behind a proxy `MALLARD_TRUST_PROXY_HEADERS=true`
plus `MALLARD_TRUSTED_PROXY_HOPS`. The unit listens on `127.0.0.1:8000` and stores
data in `/var/lib/mallard-metrics`. Back up by stopping the service and copying that
directory ([Backup and Restore](backup-restore.md)).
