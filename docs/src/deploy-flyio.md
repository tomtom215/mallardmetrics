# Fly.io Deployment

> Fly.io is a managed application platform that runs Docker containers in hardware-isolated micro-VMs (Firecracker) across a global network. It is **not** a "free tier" service — it requires a credit card. However, its Hobby plan includes enough free allowances to run Mallard Metrics at low-to-medium traffic volumes at little or no monthly cost.

> **Verification status.** This page could not be tested: it needs a Fly.io account and `flyctl`, and the repository ships **no `fly.toml`**; the one below is an example to create, not a file you can copy from the repo. What is verified is how Mallard behaves (startup checks, ports, shutdown, proxy handling, the extension download). What is **not** verified and is marked *(unverified)* where it appears: Fly's prices and allowances, volume ownership and permissions for a non-root container, the `X-Forwarded-For` Fly's proxy sends, `kill_timeout` behaviour, volume snapshot consistency, and every `fly` command. Check Fly's current documentation before relying on any of them.

---

## Overview

Fly.io runs your Docker image as a Firecracker micro-VM. Mallard Metrics deploys well because:

- The image is a small glibc distroless image (UID 65532, no shell) built from the repository `Dockerfile`; there is no musl build, because the DuckDB `behavioral` extension is not published for musl
- Fly.io provides persistent volumes for DuckDB and Parquet data
- Fly.io terminates TLS automatically — no Caddy or certbot needed
- The Fly.io edge network handles HTTP/2 and HTTPS globally
- Machines auto-start on traffic and can auto-stop when idle

**Limitations compared to a dedicated VPS:**

- No LUKS encryption (volume encryption is managed by Fly.io's infrastructure)
- Auto-stop means cold-start latency if traffic is infrequent
- Volume size and I/O throughput are lower than a dedicated disk
- Scaling beyond a single machine requires paid plan upgrades

---

## Fly.io vs VPS: When to Choose Each

| Criterion | Fly.io | Dedicated VPS |
|---|---|---|
| Setup time | < 15 minutes | 30–60 minutes |
| Monthly cost (light traffic) | ~$0–$5 | $4–$10 |
| TLS management | Automatic | Caddy (setup.sh handles) |
| Data encryption at rest | Platform-managed | LUKS (user-managed) |
| Cold-start latency | Yes (if auto-stop) | No |
| Custom kernel tuning | No | Yes |
| Multi-region | Yes | Manual |
| Persistent storage | Volumes (3 GB included) | LUKS image (you size it) |
| SSH access | `fly ssh console` | Direct SSH |

**Choose Fly.io** if you want zero infrastructure maintenance and are comfortable with platform-managed data storage.

**Choose a VPS** if you need full control, LUKS encryption, or higher data volumes.

---

## Pricing and Allowances

*(unverified: these figures are from an earlier version of this page and Fly.io has changed its plans more than once. Check [fly.io/pricing](https://fly.io/pricing) before you rely on any "free" claim.)*

The page previously described monthly allowances:

| Resource | Included free |
|---|---|
| Shared-CPU-1x 256 MB VMs | 3 VMs |
| Persistent volume storage | 3 GB |
| Outbound data transfer | 160 GB |
| TLS certificates | Unlimited |

Mallard Metrics needs:

- **1 VM** — DuckDB needs real memory: the repository's own compose files use 1 GB with `MALLARD_DUCKDB_MEMORY_LIMIT` at 640 MB. A 256 MB machine with DuckDB's default limit (about 80% of RAM) is likely to be OOM-killed by one heavy report *(untested)*. Start with 512 MB to 1 GB and set the limit explicitly.
- **1 Volume** — minimum 1 GB (DuckDB grows with data). 3 GB is comfortable for a year of moderate traffic.

At low traffic, your deployment may fit entirely within the free allowances. At higher traffic or with a large data volume, expect $1–5/month.

---

## Prerequisites

- A Fly.io account — sign up at [fly.io](https://fly.io) (credit card required)
- [flyctl](https://fly.io/docs/hands-on/install-flyctl/) installed on your local machine
- The mallardmetrics repository cloned locally
- A domain name (optional — Fly.io provides a `.fly.dev` subdomain for free)

---

## Initial Setup

### Install flyctl

**macOS:**
```bash
brew install flyctl
```

**Linux:**
```bash
curl -L https://fly.io/install.sh | sh
# Add to PATH (add this to ~/.bashrc or ~/.zshrc)
export PATH="$HOME/.fly/bin:$PATH"
```

**Windows:**
```powershell
iwr https://fly.io/install.ps1 -useb | iex
```

Verify:
```bash
fly version
```

### Authenticate

```bash
fly auth login
# Opens a browser — log in to your Fly.io account
```

---

## Configure the Application

### fly.toml

Create `fly.toml` in the repository root:

```toml
# Mallard Metrics — Fly.io configuration
# Replace "mallard-metrics-YOURNAME" with a globally unique app name.

app            = "mallard-metrics-YOURNAME"
primary_region = "ord"    # Chicago. See: fly platform regions

[build]
  # The repository Dockerfile: glibc binary on distroless, UID 65532
  dockerfile = "Dockerfile"

[env]
  # Non-secret configuration — secrets go in fly secrets (see below)
  # IMPORTANT: env var names must match config.rs exactly:
  #   MALLARD_RATE_LIMIT  → config.rate_limit_per_site  (NOT _PER_SITE suffix)
  #   MALLARD_CACHE_TTL   → config.cache_ttl_secs        (NOT _SECS suffix)
  #   MALLARD_GEOIP_DB    → config.geoip_db_path          (NOT _PATH suffix)
  MALLARD_DATA_DIR           = "/data"
  MALLARD_HOST               = "0.0.0.0"
  MALLARD_PORT               = "8080"
  MALLARD_LOG_FORMAT         = "json"
  MALLARD_FILTER_BOTS        = "true"
  MALLARD_SECURE_COOKIES     = "true"
  MALLARD_RETENTION_DAYS     = "365"
  # Fly's proxy sits in front of the app. Trust its X-Forwarded-For, one hop.
  # (unverified: confirm Fly overwrites or appends the header the way the hop
  # rule needs; see Configuration. Without these two lines every visitor looks
  # like Fly's proxy and five wrong passwords lock the admin out.)
  MALLARD_TRUST_PROXY_HEADERS = "true"
  MALLARD_TRUSTED_PROXY_HOPS  = "1"
  MALLARD_DUCKDB_MEMORY_LIMIT = "640MB"     # about 60% of a 1 GB machine
  MALLARD_SHUTDOWN_TIMEOUT    = "25"
  MALLARD_RATE_LIMIT         = "200"
  MALLARD_CACHE_TTL          = "300"
  MALLARD_MAX_LOGIN_ATTEMPTS = "5"
  MALLARD_LOGIN_LOCKOUT      = "300"
  RUST_LOG                   = "mallard_metrics=info,tower_http=warn"

[http_service]
  internal_port       = 8080
  force_https         = true       # Fly.io handles TLS; redirect HTTP → HTTPS
  auto_stop_machines  = "stop"     # Stop idle machines to save cost
  auto_start_machines = true       # Auto-start on new traffic
  min_machines_running = 1         # Keep at least 1 machine alive (prevents cold starts)
  processes            = ["app"]

  [http_service.concurrency]
    type       = "requests"
    soft_limit = 200
    hard_limit = 250

[[vm]]
  cpu_kind = "shared"
  cpus     = 1
  memory   = "1gb"       # the repository's compose files use 1 GB

# Give the shutdown flush time. Fly's default kill timeout is short (unverified:
# check the current default); Mallard may need up to MALLARD_SHUTDOWN_TIMEOUT
# for the drain and again for the final flush.
kill_timeout = "60s"

[mounts]
  source      = "mallard_data"    # Volume name (created below)
  destination = "/data"
  initial_size = "3gb"

[checks]
  [checks.health]
    grace_period = "10s"
    interval     = "30s"
    method       = "GET"
    path         = "/health/ready"
    port         = 8080
    timeout      = "5s"
    type         = "http"
```

**Choose your region** (`primary_region`):

```bash
fly platform regions
# Pick the region closest to your users or your DNS provider
# Common choices: ord (Chicago), iad (Virginia), lax (Los Angeles),
#                 lhr (London), fra (Frankfurt), nrt (Tokyo), sin (Singapore)
```

### Dockerfile note

The repository `Dockerfile` builds a glibc binary on the build host's architecture. Fly's remote builder is x86-64 by default, which matches x86-64 machines. Do not switch the Dockerfile to a musl target: the `behavioral` extension cannot load there.

The image listens on port 8000. This example overrides it to 8080 with `MALLARD_PORT`; keep `internal_port`, the health check port and `MALLARD_PORT` identical.

**Volume ownership *(unverified)*.** The container runs as UID 65532 and the image creates `/data` owned by that user, but a Fly volume is mounted over `/data`, and whether it is writable by a non-root user depends on how Fly mounts it. If the machine logs `Permission denied` or `Failed to create the data directory /data/events` at startup, the volume is not writable by UID 65532 and this deployment needs a change (for example running the machine as root, or a one-off ownership fix) that this page cannot specify.

---

## Create a Persistent Volume

The Fly.io volume stores DuckDB and Parquet data between deployments and machine restarts.

```bash
# Create a 3 GB volume in your primary region (included in Hobby allowances)
fly volumes create mallard_data \
  --size 3 \
  --region ord \
  --app mallard-metrics-YOURNAME

# Verify
fly volumes list --app mallard-metrics-YOURNAME
```

> **Important:** Volumes are single-region and single-machine by default. If you scale to multiple machines, each machine needs its own volume — but Mallard Metrics is a single-instance application (DuckDB is embedded). Do not scale to more than 1 machine without understanding the data consistency implications.

---

## Set Secrets

Fly.io secrets are encrypted at rest and injected as environment variables at runtime. Never put secrets in `fly.toml`.

```bash
APP=mallard-metrics-YOURNAME

# Required secrets — generate strong values:
fly secrets set \
  MALLARD_SECRET="$(openssl rand -base64 48)" \
  MALLARD_ADMIN_PASSWORD="$(openssl rand -base64 24 | tr -d '=+/' | head -c 32)" \
  MALLARD_METRICS_TOKEN="$(openssl rand -hex 32)" \
  --app "$APP"
```

`MALLARD_SECRET` needs 16+ characters and the admin password 12+, or the machine refuses to start. **Save the admin password and the metrics token** before running the above: neither is retrievable after setting.

```bash
# Generate and save before setting:
ADMIN_PASS="$(openssl rand -base64 24 | tr -d '=+/' | head -c 32)"
echo "Admin password: $ADMIN_PASS"  # Save this!
fly secrets set MALLARD_ADMIN_PASSWORD="$ADMIN_PASS" --app "$APP"
```

To update a secret later:
```bash
fly secrets set MALLARD_ADMIN_PASSWORD="new-password" --app "$APP"
# Fly.io triggers a rolling restart automatically
```

To view which secrets are set (names only — values are never shown):
```bash
fly secrets list --app "$APP"
```

---

## Deploy

```bash
# From the repository root directory
fly deploy --app mallard-metrics-YOURNAME

# Or launch for the first time (creates app + prompts for config):
fly launch
# Answer the prompts; Fly.io will detect the Dockerfile and suggest settings.
# Review the generated fly.toml and adjust as described above.
```

Fly.io will:

1. Build the Docker image remotely (using Fly's build infrastructure)
2. Push it to Fly.io's container registry
3. Create a Firecracker micro-VM from the image
4. Mount the `mallard_data` volume at `/data`
5. Inject secrets as environment variables
6. Start the machine and run health checks

Deployment typically takes 2–4 minutes. Watch progress:

```bash
fly deploy --app mallard-metrics-YOURNAME 2>&1 | tee deploy.log
```

---

## Configure a Custom Domain

By default your app is available at `https://mallard-metrics-YOURNAME.fly.dev`.

**To use a custom domain:**

```bash
# 1. Add the domain to your Fly.io app
fly certs add analytics.example.com --app mallard-metrics-YOURNAME

# 2. Fly.io will show you the DNS records to create:
fly certs show analytics.example.com --app mallard-metrics-YOURNAME
```

Create the DNS records shown (usually a CNAME to `<app>.fly.dev` or an A/AAAA to Fly's IPs). Fly.io obtains a Let's Encrypt certificate automatically via the HTTP-01 or DNS-01 challenge.

**Update the app to know its domain:**

```bash
fly secrets set \
  MALLARD_DASHBOARD_ORIGIN="https://analytics.example.com" \
  --app mallard-metrics-YOURNAME
```

---

## Verify the Deployment

```bash
# Check machine status
fly status --app mallard-metrics-YOURNAME

# View machine health
fly checks list --app mallard-metrics-YOURNAME

# Quick smoke test
curl -s https://mallard-metrics-YOURNAME.fly.dev/health/ready
# Expected: ready

# View all available endpoints
curl -s https://mallard-metrics-YOURNAME.fly.dev/health/detailed | jq .
```

Open `https://mallard-metrics-YOURNAME.fly.dev` (or your custom domain) in a browser. Log in with the admin password you set.

---

## Logs and Monitoring

```bash
# Stream live logs
fly logs --app mallard-metrics-YOURNAME

# Historical logs (last N lines)
fly logs --app mallard-metrics-YOURNAME -n 200

# Parse JSON structured logs
fly logs --app mallard-metrics-YOURNAME | jq 'select(.fields.uri != "/health/ready")'

# Machine console (unverified). The image has no shell, so what you can do in
# there depends on what Fly's console provides.
fly ssh console --app mallard-metrics-YOURNAME
```

**Prometheus metrics:**
```bash
# `fly secrets list` shows only digests, so use the token you saved when you set it:
curl -H "Authorization: Bearer $MALLARD_METRICS_TOKEN" \
  https://mallard-metrics-YOURNAME.fly.dev/metrics
```

**Fly.io built-in monitoring:**

The Fly.io dashboard at [fly.io/apps/YOUR-APP](https://fly.io/apps) shows:
- Machine CPU and memory graphs
- HTTP request rate and latency
- Health check pass/fail history

---

## Scaling and Regions

**Increase VM memory** (if DuckDB queries are slow or OOMing):
```bash
# Edit fly.toml:
# [[vm]]
#   memory = "512mb"   # or "1gb"

fly deploy  # Apply the change
```

**Prevent cold starts** (machine auto-stops when idle):
```bash
# In fly.toml, ensure:
# [http_service]
#   min_machines_running = 1
```

This keeps 1 machine always running, eliminating cold-start latency at the cost of ~1 machine's worth of compute (within Hobby allowances).

**Multi-region** (advanced):

Fly.io supports deploying machines in multiple regions for lower global latency. However, Mallard Metrics uses an embedded single-file DuckDB database — volumes cannot be shared across regions. Multi-region deployment is not recommended without a replication strategy.

---

## Updating Mallard Metrics

```bash
# Pull latest changes
git pull origin main

# Deploy (Fly.io builds the new image and does a rolling restart)
fly deploy --app mallard-metrics-YOURNAME

# Monitor the deploy
fly status --app mallard-metrics-YOURNAME
fly logs --app mallard-metrics-YOURNAME
```

Fly.io performs a blue/green-style deploy — it starts the new machine, runs health checks, and only terminates the old machine once the new one is healthy. Downtime is typically < 5 seconds.

---

## Backups

Read [Backup and Restore](backup-restore.md) first: the data directory holds a live DuckDB file that cannot be copied consistently while the process runs, and `deploy/backup.sh` is written for Docker Compose, not Fly.

**The export endpoint is not a backup.** `GET /api/stats/export` returns daily rollups or raw events *without visitor IDs*, for one site and date range, up to a row limit. It is useful for analysis, not for restoring an instance (it excludes the admin hash, API keys and secret).

**Volume snapshot** (Fly.io feature; *unverified for DuckDB consistency*: a snapshot taken while Mallard runs is a crash-consistent image of the volume at best, and the guidance above says a live copy may not open or may miss recent writes. Mallard recovers interrupted Parquet flushes at startup but the DuckDB file and WAL are what is at risk. To get a clean snapshot, stop the machine first, snapshot, then start it):
```bash
# List volumes
fly volumes list --app mallard-metrics-YOURNAME

# Create a snapshot (may cause brief I/O pause)
fly volumes snapshots create <VOLUME_ID> --app mallard-metrics-YOURNAME

# List snapshots
fly volumes snapshots list <VOLUME_ID> --app mallard-metrics-YOURNAME
```

**Restore from snapshot:**
```bash
# Create a new volume from snapshot
fly volumes create mallard_data_restore \
  --snapshot-id <SNAPSHOT_ID> \
  --size 3 \
  --region ord \
  --app mallard-metrics-YOURNAME
```

---

## Troubleshooting

### Machine fails to start

```bash
fly logs --app mallard-metrics-YOURNAME | tail -50

# Common causes:
# 1. MALLARD_SECRET missing or under 16 characters, or the admin password under 12 — run: fly secrets list; the log's first line names it
# 2. Volume not found — run: fly volumes list
# 3. Port mismatch — ensure MALLARD_PORT=8080 matches fly.toml internal_port=8080
```

### Health checks failing

```bash
fly checks list --app mallard-metrics-YOURNAME

# Test the endpoint manually. The image has no shell or wget, so use a proxy:
fly proxy 8080 --app mallard-metrics-YOURNAME
# Then in another terminal: curl http://localhost:8080/health/ready
```

### Volume not mounted / data missing after update

```bash
# Check the mount
fly ssh console --app mallard-metrics-YOURNAME
ls /data/

# If /data is empty, the volume may have been detached
# Verify volume attachment in fly.toml [mounts] section matches the volume name
fly volumes list --app mallard-metrics-YOURNAME
```

### Out of disk space on volume

```bash
# Extend the volume (Fly.io allows online resize)
fly volumes extend <VOLUME_ID> --size 10 --app mallard-metrics-YOURNAME

# Enable retention to prune old data
fly secrets set MALLARD_RETENTION_DAYS=180 --app mallard-metrics-YOURNAME
```

### Machine auto-stopped unexpectedly

```bash
# Check if auto_stop_machines is enabled in fly.toml
# Ensure min_machines_running = 1 to prevent full auto-stop

# Or disable auto-stop entirely:
# [http_service]
#   auto_stop_machines = false
```

---

## Frequently Asked Questions

**Q: Does Fly.io encrypt volume data at rest?**

Yes — Fly.io encrypts all volume data at rest using AES-256. You do not need to manage LUKS yourself. For compliance requirements, consult [Fly.io's security documentation](https://fly.io/docs/security/).

**Q: Do I need a credit card?**

Yes. Fly.io requires a payment method for all accounts, including those that stay within the free allowances. There is no truly card-free free tier.

**Q: What is the cold-start latency?**

When `auto_stop_machines = "stop"` and `min_machines_running = 0`, an idle machine is stopped after ~5 minutes. The first request after that triggers a cold start — typically 2–5 seconds for the Firecracker VM to boot. For an analytics ingestion endpoint, this means some requests may be delayed or dropped during cold start. Set `min_machines_running = 1` to keep the machine always warm.

**Q: Can I use Fly.io without a custom domain?**

Yes. Fly.io provides a free `*.fly.dev` subdomain with a valid TLS certificate. Use it in your tracking script and dashboard URL.

**Q: How do I SSH into the machine?**

`fly ssh console --app mallard-metrics-YOURNAME`

*(unverified)* The Mallard image is distroless and has no shell, so it cannot be exec'd into. Whether `fly ssh console` gives you a usable shell on the VM, and where the volume is visible from it, depends on Fly. Prefer `/health/detailed` and the logs.

**Q: Can I run Mallard Metrics alongside other services?**

Fly.io apps are isolated. You can deploy other services as separate Fly apps in the same organisation and they share the same billing account. Each service gets its own machine(s) and volume(s).

**Q: How do I migrate from Fly.io to a VPS?**

1. Export your data via the API (`/api/stats/export`)
2. Or copy the volume contents: create a volume snapshot, restore it locally
3. Copy `mallard.duckdb` and the Parquet data directory to your VPS LUKS volume
4. Follow the [VPS Deployment Guide](deploy-vps.md)

**Q: Does the behavioral extension work on Fly.io?**

Yes. Mallard downloads the extension on first start onto the volume (`/data/extensions`), which needs outbound access to `community-extensions.duckdb.org`; a Fly machine normally has it *(unverified)*. Check `GET /health/detailed`: `"behavioral_extension_loaded": true` confirms it. Later starts load it from the volume. The image must be glibc (it is); a musl image could never load it.

**Q: What happens to in-flight events if the machine is auto-stopped?**

Mallard handles `SIGTERM` with a graceful shutdown — it flushes the in-memory event buffer to Parquet before the machine stops. As long as the machine is given enough time to finish (Mallard drains connections for up to `MALLARD_SHUTDOWN_TIMEOUT` seconds, default 30, then flushes for up to that long again; set `kill_timeout` accordingly, *unverified*), no events are lost. If the platform kills the process first, events still in the memory buffer are lost. Set `min_machines_running = 1` to avoid auto-stop entirely for high-reliability deployments.
