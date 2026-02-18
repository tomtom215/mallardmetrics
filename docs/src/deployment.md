# Deployment

## Production Checklist

Before going to production:

- [ ] Set `MALLARD_SECRET` to a random 32+ character string (and keep it constant across restarts).
- [ ] Set `MALLARD_ADMIN_PASSWORD` to a strong password.
- [ ] Configure a TLS-terminating reverse proxy (nginx, Caddy, Traefik).
- [ ] Mount a persistent volume for `data_dir`.
- [ ] Set `site_ids` to restrict event ingestion to your domains.
- [ ] Configure `retention_days` to match your data retention policy.

---

## Docker (Recommended)

### Pull and Run

```bash
docker run -d \
  --name mallard-metrics \
  --restart unless-stopped \
  -p 127.0.0.1:8000:8000 \
  -v mallard-data:/data \
  -e MALLARD_SECRET=your-random-32-char-secret \
  -e MALLARD_ADMIN_PASSWORD=your-dashboard-password \
  ghcr.io/tomtom215/mallard-metrics
```

The image is built `FROM scratch` with a static musl binary. It has no shell, no package manager, and no runtime dependencies.

### With a Config File

```bash
docker run -d \
  --name mallard-metrics \
  -v mallard-data:/data \
  -v /etc/mallard-metrics/config.toml:/config.toml:ro \
  -e MALLARD_SECRET=... \
  -e MALLARD_ADMIN_PASSWORD=... \
  ghcr.io/tomtom215/mallard-metrics /config.toml
```

---

## Docker Compose

Save the following as `docker-compose.yml`:

```yaml
services:
  mallard-metrics:
    image: ghcr.io/tomtom215/mallard-metrics:latest
    restart: unless-stopped
    ports:
      - "127.0.0.1:8000:8000"
    volumes:
      - mallard-data:/data
    environment:
      MALLARD_SECRET: "${MALLARD_SECRET}"
      MALLARD_ADMIN_PASSWORD: "${MALLARD_ADMIN_PASSWORD}"
      MALLARD_LOG_FORMAT: "json"

volumes:
  mallard-data:
```

Create a `.env` file (do not commit to source control):

```bash
MALLARD_SECRET=your-random-32-char-secret
MALLARD_ADMIN_PASSWORD=your-dashboard-password
```

Start:

```bash
docker compose up -d
docker compose logs -f
```

---

## Behind a Reverse Proxy

Mallard Metrics binds to `127.0.0.1:8000` by default when run locally. Configure your reverse proxy to forward requests.

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
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

> **Important:** Mallard Metrics reads the client IP for visitor ID hashing. If behind a proxy, the `X-Forwarded-For` or `X-Real-IP` header must be set correctly. Configure your proxy to send the real client IP.

### Caddy

```caddy
analytics.example.com {
    reverse_proxy 127.0.0.1:8000
}
```

Caddy sets `X-Forwarded-For` automatically.

---

## Build from Source (Static musl Binary)

To build a `FROM scratch`-compatible static binary:

```bash
# Install the musl target
rustup target add x86_64-unknown-linux-musl

# Build
cargo build --release --target x86_64-unknown-linux-musl

# The binary
ls -lh target/x86_64-unknown-linux-musl/release/mallard-metrics
```

The resulting binary has no dynamic library dependencies:

```bash
ldd target/x86_64-unknown-linux-musl/release/mallard-metrics
# not a dynamic executable
```

---

## GeoIP Setup

Mallard Metrics supports optional IP geolocation via MaxMind GeoLite2.

1. Create a free account at [maxmind.com](https://www.maxmind.com/en/geolite2/signup).
2. Download the `GeoLite2-City.mmdb` database.
3. Configure the path:

```toml
# config.toml
geoip_db_path = "/data/GeoLite2-City.mmdb"
```

Or with Docker:

```bash
docker run ... \
  -v /path/to/GeoLite2-City.mmdb:/data/GeoLite2-City.mmdb:ro \
  -e ... \
  ghcr.io/tomtom215/mallard-metrics
```

If the file is missing or unreadable, country/region/city fields are stored as `NULL`. No error is raised.

> **Note:** The MaxMind GeoLite2 database is updated monthly. Automate downloads with [geoipupdate](https://github.com/maxmind/geoipupdate).

---

## Graceful Shutdown

Mallard Metrics handles `SIGINT` (Ctrl+C) and `SIGTERM` (Docker stop, systemd stop). On receiving either signal:

1. The server stops accepting new connections.
2. In-flight requests are completed.
3. Buffered events are flushed to Parquet.

The flush is bounded by `shutdown_timeout_secs` (default 30). If flushing takes longer, a warning is logged and the process exits.

---

## Systemd Service

For non-Docker deployments:

```ini
[Unit]
Description=Mallard Metrics
After=network.target

[Service]
Type=simple
User=mallard
ExecStart=/usr/local/bin/mallard-metrics /etc/mallard-metrics/config.toml
Restart=on-failure
RestartSec=5s
Environment=MALLARD_SECRET=...
Environment=MALLARD_ADMIN_PASSWORD=...

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload
systemctl enable --now mallard-metrics
```
