# VPS Deployment Guide

> **Zero to production in one command** — deploy Mallard Metrics on any generic Linux VPS with full TLS, LUKS-encrypted data at rest, Cloudflare DNS, and an automated security audit.

---

## Overview

This guide deploys Mallard Metrics on a bare VPS using:

| Component | Role |
|---|---|
| **Caddy** (custom build) | TLS termination, reverse proxy, HTTP/3, ACME DNS-01 |
| **Cloudflare DNS** | DNS-01 ACME challenge — no port 80 required |
| **LUKS** | Full encryption of the analytics data volume at rest |
| **Docker Compose** | Container orchestration |
| **vps-audit** | Automated security assessment and weekly re-audit |
| **UFW + fail2ban** | Host-level firewall and brute-force protection |

The `FROM scratch` Mallard binary runs with no shell, no OS utilities, read-only root filesystem, all Linux capabilities dropped, and no network port exposed to the host — all traffic flows through Caddy on the internal Docker network.

---

## Architecture

```
Internet
    │
    ▼
┌───────────────────────────────────────────────┐
│  VPS Host (Ubuntu/Debian)                     │
│                                               │
│  UFW Firewall: 22, 80, 443 (tcp+udp/QUIC)    │
│                                               │
│  ┌─────────────────────────────────────────┐  │
│  │  Docker network: mallard-production_proxy│  │
│  │                                         │  │
│  │  ┌─────────────┐    ┌───────────────┐   │  │
│  │  │   Caddy     │───▶│ mallard:8000  │   │  │
│  │  │ :80/:443    │    │ (FROM scratch) │   │  │
│  │  │ TLS + proxy │    │               │   │  │
│  │  └─────────────┘    └───────┬───────┘   │  │
│  └───────────────────────────  │ ──────────┘  │
│                                │              │
│  ┌─────────────────────────────▼────────────┐ │
│  │  LUKS encrypted volume (/srv/mallard/data)│ │
│  │  mallard.duckdb  data/YYYY/MM/DD/*.parquet│ │
│  └───────────────────────────────────────────┘ │
└───────────────────────────────────────────────┘
```

---

## Prerequisites

### VPS requirements

| Resource | Minimum | Recommended |
|---|---|---|
| CPU | 1 vCPU | 2 vCPU |
| RAM | 512 MB | 1 GB |
| Disk | 10 GB | 40 GB |
| OS | Ubuntu 22.04 | Ubuntu 24.04 LTS |
| Architecture | x86-64 | x86-64 |

Mallard Metrics is a single static binary. Under light to medium traffic (< 50k daily events) the minimum spec is adequate. The disk budget is dominated by Parquet data growth and the LUKS image pre-allocation.

### Domain and DNS

You need a domain whose DNS is managed in Cloudflare. The domain can be:

- **A subdomain**: `analytics.example.com` (recommended — keeps the apex clean)
- **An apex domain**: `example.com`

Create an **A record** pointing to your VPS IP before running setup. Caddy validates DNS during certificate issuance.

```
analytics.example.com.  A  203.0.113.42
```

If you're using Cloudflare's proxy (orange cloud), set it to **DNS only (grey cloud)** for the analytics subdomain. Caddy manages TLS itself and Cloudflare's proxy can interfere with HTTP/3 and certificate validation.

### Cloudflare API token

Caddy uses the Cloudflare API to create DNS TXT records for ACME DNS-01 challenges. Create a scoped token:

1. Log in to [dash.cloudflare.com](https://dash.cloudflare.com) → **My Profile** → **API Tokens**
2. Click **Create Token** → **Custom Token**
3. Set permissions:
   - **Zone → Zone → Read** (for all zones or just the specific zone)
   - **Zone → DNS → Edit** (for the specific zone containing your domain)
4. Restrict to **Zone Resources → Specific zone → your zone**
5. Copy the generated token — you will not see it again

### SSH key access

`setup.sh` disables SSH password authentication as part of hardening. **You must have an SSH public key installed on the server before running the script**, or you will be locked out.

```bash
# On your local machine — copy your public key to the server
ssh-copy-id -i ~/.ssh/id_ed25519.pub user@your-vps-ip

# Verify it works before running setup
ssh -i ~/.ssh/id_ed25519 user@your-vps-ip echo "Key access confirmed"
```

---

## One-Command Deployment

If you trust the script (review it first), this does everything:

```bash
# 1. SSH into the VPS
ssh user@your-vps-ip

# 2. Clone the repository
git clone https://github.com/tomtom215/mallardmetrics.git
cd mallardmetrics

# 3. Run the setup script
sudo bash deploy/setup.sh
```

The script is interactive — it will prompt for your domain, email, and Cloudflare API token, then generate and display the admin password.

**Pre-set values to run non-interactively** (e.g., for CI/cloud-init):

```bash
export MM_DOMAIN=analytics.example.com
export MM_EMAIL=admin@example.com
export MM_CF_TOKEN=your-cloudflare-token
sudo -E bash deploy/setup.sh
```

---

## Step-by-Step Manual Deployment

### Step 1 — Provision the VPS

Choose a provider (any KVM/XEN VPS works):

- [Hetzner](https://www.hetzner.com) — CX22 (2 vCPU, 4 GB, €4/mo) is excellent value
- [DigitalOcean](https://www.digitalocean.com) — Basic Droplet $6/mo
- [Vultr](https://www.vultr.com) — Cloud Compute $5/mo
- [Oracle Cloud Always Free](https://www.oracle.com/cloud/free/) — 2 AMD VMs, 200 GB block storage, genuinely free
- [Linode/Akamai](https://www.linode.com) — Shared CPU $5/mo

Use **Ubuntu 22.04 LTS** or **24.04 LTS** as the OS image. Enable backups at the provider level for an additional safety net.

After provisioning:

```bash
# Note your VPS IP address, then SSH in
ssh root@<VPS-IP>

# Immediately create a non-root user with sudo
adduser deploy
usermod -aG sudo deploy

# Add your SSH key to the new user
mkdir -p /home/deploy/.ssh
cp /root/.ssh/authorized_keys /home/deploy/.ssh/
chown -R deploy:deploy /home/deploy/.ssh
chmod 700 /home/deploy/.ssh
chmod 600 /home/deploy/.ssh/authorized_keys

# Switch to the non-root user for the rest
su - deploy
```

### Step 2 — Clone the repository

```bash
git clone https://github.com/tomtom215/mallardmetrics.git
cd mallardmetrics
```

### Step 3 — Run setup.sh

```bash
sudo bash deploy/setup.sh
```

The script will:

1. Detect your OS and verify prerequisites
2. Ask you to confirm SSH key access before hardening SSH
3. Update packages and install tooling
4. Harden SSH, enable UFW firewall, configure fail2ban
5. Apply kernel hardening sysctl settings
6. Install Docker CE and the Compose plugin
7. Create a 20 GB LUKS-encrypted image at `/srv/mallard/data.img` and mount it
8. Download and run [vps-audit](https://github.com/tomtom215/vps-audit) — saving the report
9. Prompt you to configure `deploy/.env` (or auto-generate secrets)
10. Build the Docker images and start the stack
11. Install weekly vps-audit and daily backup cron jobs
12. Print your admin password and a post-setup checklist

### Step 4 — Verify deployment

```bash
# Check container status
docker compose -f deploy/docker-compose.production.yml ps

# Check Caddy got a certificate (look for "TLS certificate obtained")
docker compose -f deploy/docker-compose.production.yml logs caddy | grep -i cert

# Test the health endpoint (replace with your domain)
curl -s https://analytics.example.com/health/ready

# Expected: ready
```

Open `https://<your-domain>` in a browser. You should see the Mallard Metrics dashboard login page.

---

## What setup.sh Does

Here is the complete sequence of operations `setup.sh` performs, with the rationale for each:

| Step | Operation | Why |
|---|---|---|
| 1 | OS detection and SSH key check | Prevents lockout before hardening |
| 2 | `apt upgrade` + `unattended-upgrades` | Patches known CVEs immediately |
| 3 | SSH drop-in config in `sshd_config.d/` | Non-destructive; preserves original config |
| 4 | UFW: deny-all ingress, allow 22/80/443 | Minimal attack surface |
| 5 | fail2ban for SSH | Blocks brute-force login attempts |
| 6 | Kernel sysctl hardening | Disables TCP redirects, restricts dmesg/BPF |
| 7 | Docker CE from official repo | Ensures a current, vendor-supported version |
| 8 | LUKS encrypted image + keyfile | Analytics data encrypted at rest |
| 9 | vps-audit + weekly cron | Ongoing visibility into security posture |
| 10 | Secret generation + `deploy/.env` | Strong random credentials without manual work |
| 11 | `docker compose build && up -d` | Brings the stack live |
| 12 | Backup cron (rsync) | Daily snapshot of DuckDB + Parquet |

---

## LUKS Encrypted Volume

### How it works

`setup.sh` creates a file-backed LUKS2 container at `/srv/mallard/data.img` using AES-XTS-PLAIN64 with a 512-bit key. A random keyfile is stored at `/etc/mallard-data.key` (read-only by root) so the volume auto-unlocks on boot without a passphrase prompt.

The decrypted volume is formatted ext4 and mounted at `/srv/mallard/data`. The Mallard Metrics container bind-mounts this path as `/data`.

```
/srv/mallard/data.img   ← LUKS2 container (AES-256 XTS, file on host disk)
        ↓ cryptsetup luksOpen
/dev/mapper/mallard-data  ← Decrypted block device
        ↓ ext4 mount
/srv/mallard/data/        ← Plaintext filesystem (only visible to root while mounted)
        ↓ Docker bind mount
/data/ (inside container) ← mallard.duckdb, data/YYYY/MM/DD/*.parquet
```

If an attacker gains access to the raw disk image (e.g., by stealing a disk or snapshot), the data is unreadable without the keyfile.

### After reboot

The LUKS volume is configured in `/etc/crypttab` and `/etc/fstab` to auto-mount on boot using the keyfile. No manual intervention is required after a planned reboot.

```bash
# To verify the volume mounted after a reboot:
mountpoint /srv/mallard/data && echo "mounted" || echo "NOT mounted"

# If it did not mount (e.g., keyfile missing), mount manually:
sudo cryptsetup luksOpen --key-file /etc/mallard-data.key \
    /srv/mallard/data.img mallard-data
sudo mount /dev/mapper/mallard-data /srv/mallard/data

# Then restart the stack
sudo docker compose -f /path/to/mallardmetrics/deploy/docker-compose.production.yml up -d
```

### Resizing the volume

```bash
# 1. Stop the stack
docker compose -f deploy/docker-compose.production.yml down

# 2. Unmount and close
sudo umount /srv/mallard/data
sudo cryptsetup luksClose mallard-data

# 3. Grow the image file (+10 GB example)
sudo fallocate -l 30G /srv/mallard/data.img          # change to new total size

# 4. Grow the LUKS container
sudo cryptsetup luksOpen --key-file /etc/mallard-data.key \
    /srv/mallard/data.img mallard-data
sudo cryptsetup resize mallard-data

# 5. Grow the filesystem
sudo e2fsck -f /dev/mapper/mallard-data
sudo resize2fs /dev/mapper/mallard-data

# 6. Re-mount and restart
sudo mount /dev/mapper/mallard-data /srv/mallard/data
docker compose -f deploy/docker-compose.production.yml up -d
```

---

## Caddy and TLS

### Cloudflare DNS challenge

The `Caddyfile` is configured for the ACME DNS-01 challenge using the Cloudflare provider. This means:

- **Port 80 does not need to be accessible** — challenge is completed via DNS API
- **Wildcard certificates** (`*.example.com`) are supported
- **Certificates are obtained before the first request** arrives

Caddy stores its ACME account and certificates in the `caddy-data` Docker volume. Certificates are renewed automatically, typically 30 days before expiry.

### Certificate renewal

No action is required — Caddy handles renewal entirely. To check certificate status:

```bash
# View Caddy's certificate store
docker exec mallard-caddy caddy environ
docker exec mallard-caddy caddy list-modules | grep dns

# Check cert expiry
echo | openssl s_client -connect analytics.example.com:443 -servername analytics.example.com 2>/dev/null \
    | openssl x509 -noout -dates
```

### Custom domain configurations

**Subdomain (most common):**
```
# In .env
DOMAIN=analytics.example.com
```

**Apex domain:**
```
# In .env
DOMAIN=example.com
```

**Multiple domains** (edit `deploy/Caddyfile` directly):
```
analytics.example.com, stats.myothersite.io {
    import security_headers
    reverse_proxy mallard:8000 { ... }
}
```

---

## Security Hardening

### vps-audit integration

[vps-audit](https://github.com/tomtom215/vps-audit) performs 40+ security checks across SSH, firewall, kernel, authentication, file permissions, and services.

```bash
# Run a fresh audit at any time
sudo vps-audit

# Run with JSON output for automation
sudo vps-audit --format json > /tmp/audit.json

# View the initial audit report
cat /srv/mallard/vps-audit-initial-$(date +%Y%m%d).log

# View weekly audit logs
tail -100 /var/log/vps-audit.log
```

The weekly cron runs every Sunday at 03:00 UTC. Review WARN and FAIL items and address them using the audit's built-in guidance (`vps-audit --guide`).

### SSH hardening

`setup.sh` installs a hardening drop-in at `/etc/ssh/sshd_config.d/99-mallard-hardening.conf`:

```
PermitRootLogin no           # Root cannot SSH in at all
PasswordAuthentication no    # Only public key authentication
MaxAuthTries 3               # Lock after 3 failed attempts
LoginGraceTime 30            # 30s window to authenticate
ClientAliveInterval 300      # 5-minute keepalive
AllowAgentForwarding no      # No agent forwarding
AllowTcpForwarding no        # No tunnel forwarding
X11Forwarding no             # No graphical forwarding
```

fail2ban bans IPs after 5 failed SSH attempts for 1 hour.

### Firewall (UFW)

```bash
# View current rules
sudo ufw status numbered

# Default policy after setup.sh
# Default incoming: deny
# Default outgoing: allow
# 22/tcp  — SSH
# 80/tcp  — HTTP (Caddy redirects to HTTPS)
# 443/tcp — HTTPS
# 443/udp — HTTP/3 QUIC
```

### Kernel parameters

Applied via `/etc/sysctl.d/99-mallard-hardening.conf`:

| Setting | Value | Effect |
|---|---|---|
| `tcp_syncookies` | 1 | SYN flood protection |
| `rp_filter` | 1 | Spoofed packet rejection |
| `accept_redirects` | 0 | ICMP redirect attacks blocked |
| `dmesg_restrict` | 1 | Kernel log visible only to root |
| `unprivileged_bpf_disabled` | 1 | BPF restricted to privileged users |
| `bpf_jit_harden` | 2 | JIT hardening against side-channel |
| `suid_dumpable` | 0 | No core dumps from setuid programs |

---

## Configuration Reference

All configuration is in `deploy/.env`. The file is created by `setup.sh` from `deploy/.env.example`. Here are the settings most commonly adjusted post-deployment:

| Variable | Default | Description |
|---|---|---|
| `DOMAIN` | _(required)_ | Hostname Caddy serves |
| `MALLARD_RETENTION_DAYS` | `365` | Delete Parquet partitions older than N days |
| `MALLARD_RATE_LIMIT` | `0` (unlimited) | Max events/sec per site_id |
| `MALLARD_CACHE_TTL` | `60` | Query result cache TTL (seconds) |
| `MALLARD_MAX_CONCURRENT_QUERIES` | `10` | DuckDB concurrency cap |
| `MALLARD_MAX_LOGIN_ATTEMPTS` | `5` | Failed logins before IP lockout |
| `MALLARD_LOGIN_LOCKOUT` | `300` | Lockout duration (seconds) |
| `MALLARD_GEOIP_DB` | _(blank)_ | Path to MaxMind GeoLite2-City.mmdb (inside container) |

After editing `.env`, restart the stack:

```bash
docker compose -f deploy/docker-compose.production.yml up -d
```

---

## Adding the Tracking Script

Add this to every page you want to track:

```html
<script
  defer
  src="https://analytics.example.com/mallard.js"
  data-domain="example.com">
</script>
```

Replace `analytics.example.com` with your deployment domain and `example.com` with the site_id you want to use for this site.

**Custom events:**
```javascript
window.mallard('Purchase', {
  revenue: '49.99',
  currency: 'USD',
  props: JSON.stringify({ plan: 'pro' })
});
```

**Embed on GitHub Pages docs** (static site):

Simply paste the `<script>` tag into your `mdBook` layout template or into individual markdown pages using HTML passthrough. The script is < 1 KB and has zero external dependencies.

---

## Accessing the Dashboard Remotely

The dashboard is served at the root URL of your Mallard Metrics instance (e.g. `https://analytics.example.com`). It requires authentication when `MALLARD_ADMIN_PASSWORD` is set.

> **Note:** The server sets `X-Frame-Options: DENY` to prevent clickjacking, so the dashboard cannot be embedded in an iframe. Access it directly in a browser tab instead.

---

## Post-Deployment Operations

### View logs

```bash
# All services (follow)
docker compose -f deploy/docker-compose.production.yml logs -f

# Mallard only (JSON structured logs)
docker compose -f deploy/docker-compose.production.yml logs mallard | jq .

# Caddy access log (on the LUKS volume)
tail -f /srv/mallard/data/logs/caddy-access.log | jq .
```

### Update Mallard Metrics

```bash
cd ~/mallardmetrics

# Pull latest changes
git pull origin main

# Rebuild and restart (zero downtime if only Mallard changes)
docker compose -f deploy/docker-compose.production.yml build mallard
docker compose -f deploy/docker-compose.production.yml up -d mallard

# Or rebuild everything
docker compose -f deploy/docker-compose.production.yml build --no-cache
docker compose -f deploy/docker-compose.production.yml up -d
```

> The Caddy build only needs to be rebuilt if you change `deploy/Dockerfile.caddy` or `deploy/Caddyfile`.

### Backup and restore

**Backup** (done automatically daily by the cron job):
```bash
# Manual backup
rsync -a --delete /srv/mallard/data/ /srv/mallard/backup/

# Copy off-server (replace with your backup destination)
rsync -az /srv/mallard/data/ backup-server:/backups/mallard/$(date +%Y%m%d)/
```

**Restore:**
```bash
# Stop the stack
docker compose -f deploy/docker-compose.production.yml down

# Restore data files
rsync -a /srv/mallard/backup/ /srv/mallard/data/

# Restart
docker compose -f deploy/docker-compose.production.yml up -d
```

### GeoIP setup

Mallard supports MaxMind GeoLite2-City for country/region/city resolution.

1. Create a free MaxMind account at [maxmind.com](https://www.maxmind.com)
2. Download `GeoLite2-City.mmdb`
3. Copy it to the data volume:
   ```bash
   cp GeoLite2-City.mmdb /srv/mallard/data/GeoLite2-City.mmdb
   ```
4. Update `deploy/.env`:
   ```
   MALLARD_GEOIP_DB=/data/GeoLite2-City.mmdb
   ```
5. Restart Mallard:
   ```bash
   docker compose -f deploy/docker-compose.production.yml restart mallard
   ```

Set up weekly automatic updates (MaxMind databases are updated Tuesdays and Fridays):

```bash
# Install geoipupdate
apt-get install -y geoipupdate

# Configure with your MaxMind account ID and licence key
# /etc/GeoIP.conf:
# AccountID YOUR_ACCOUNT_ID
# LicenseKey YOUR_LICENSE_KEY
# EditionIDs GeoLite2-City

# Run update
geoipupdate

# Link to the data volume
ln -sf /usr/share/GeoIP/GeoLite2-City.mmdb /srv/mallard/data/GeoLite2-City.mmdb
```

---

## Monitoring

The detailed health endpoint returns rich status JSON:

```bash
curl -s https://analytics.example.com/health/detailed | jq .
```

Example response:
```json
{
  "status": "ok",
  "version": "0.1.0",
  "buffered_events": 0,
  "auth_configured": true,
  "geoip_loaded": false,
  "behavioral_extension_loaded": true,
  "filter_bots": true,
  "cache_entries": 0,
  "cache_empty": true
}
```

**Prometheus metrics** (requires `MALLARD_METRICS_TOKEN`):

```bash
curl -H "Authorization: Bearer $MALLARD_METRICS_TOKEN" \
  https://analytics.example.com/metrics
```

Available metrics:
- `mallard_events_ingested_total` — cumulative event count
- `mallard_flush_failures_total` — Parquet flush failures
- `mallard_rate_limit_rejections_total` — rate-limited requests
- `mallard_login_failures_total` — failed dashboard logins
- `mallard_cache_hits_total` / `mallard_cache_misses_total` — query cache
- `mallard_behavioral_extension` — 1 if the behavioral extension loaded

**UptimeRobot / Better Uptime:**

Monitor `https://<domain>/health/ready` with a 1-minute interval. It returns HTTP 200 when the database is reachable, 503 otherwise.

---

## Troubleshooting

### Caddy shows "certificate error" or HTTP instead of HTTPS

```bash
# Check Caddy logs for ACME errors
docker compose -f deploy/docker-compose.production.yml logs caddy | grep -i "acme\|cert\|error"

# Common causes:
# 1. CLOUDFLARE_API_TOKEN is wrong or lacks Zone:DNS:Edit permission
# 2. DNS A record not yet propagated (allow up to 10 minutes)
# 3. You hit Let's Encrypt rate limits — wait 1 hour or switch to staging
#    (uncomment the acme_ca staging line in deploy/Caddyfile)
```

### Mallard container exits immediately

```bash
docker compose -f deploy/docker-compose.production.yml logs mallard

# Common cause: MALLARD_SECRET is blank (required at startup)
# Check deploy/.env has MALLARD_SECRET set to a non-empty value
```

### Data volume not mounted after reboot

```bash
# Check if LUKS device is open
ls -la /dev/mapper/mallard-data || echo "LUKS device not open"

# Check mount
mountpoint /srv/mallard/data || echo "Not mounted"

# Manually open and mount
sudo cryptsetup luksOpen --key-file /etc/mallard-data.key \
    /srv/mallard/data.img mallard-data
sudo mount /dev/mapper/mallard-data /srv/mallard/data

# Restart stack
docker compose -f deploy/docker-compose.production.yml up -d
```

### Health check returns 503

```bash
# Mallard is running but the DuckDB VIEW rebuild failed
docker compose -f deploy/docker-compose.production.yml logs mallard | tail -50

# Try restarting Mallard only (Caddy stays up, no TLS interruption)
docker compose -f deploy/docker-compose.production.yml restart mallard
```

### Port 443 already in use

```bash
sudo ss -tlnp | grep :443
# If another process (nginx, apache) is listening:
sudo systemctl stop nginx apache2 2>/dev/null || true
docker compose -f deploy/docker-compose.production.yml up -d caddy
```

### Out of disk space

```bash
df -h /srv/mallard/data   # Check LUKS volume usage
df -h /var/lib/docker     # Check Docker overlay usage

# Trim old Docker layers
docker system prune -f

# Enable data retention if not already set
# In deploy/.env: MALLARD_RETENTION_DAYS=365
# Then restart mallard
```

---

## Frequently Asked Questions

**Q: Can I deploy without Cloudflare?**

Yes — use any DNS provider that Caddy supports. The DNS-01 plugin ecosystem includes Route53, GoDaddy, Namecheap, Gandi, and many others. See [caddyserver.com/docs/modules/dns](https://caddyserver.com/docs) for the full list. Alternatively, if port 80 is accessible from the internet, change the `Caddyfile` global block to remove `acme_dns` and Caddy will use the HTTP-01 challenge automatically.

**Q: Can I run Mallard Metrics on a Raspberry Pi or ARM server?**

The current `Dockerfile` targets `x86_64-unknown-linux-musl`. To build for ARM64, change the target to `aarch64-unknown-linux-musl` in the `Dockerfile` and add `platform: linux/arm64` to the compose service. The rest of the stack (Caddy, LUKS) is architecture-agnostic.

**Q: How do I add multiple sites?**

Mallard Metrics handles multiple sites with a single deployment. Each site uses a different `data-domain` in the tracking script. All data is partitioned by `site_id` at the Parquet layer. Dashboard queries are filtered per site.

**Q: Is the LUKS keyfile approach secure?**

The keyfile provides encryption at rest — protection against an attacker who obtains the raw disk image (e.g., a stolen drive or a cloud snapshot). It does **not** protect against an attacker who has live root access to a running server, because the decrypted volume is mounted and readable. For higher threat models, use a passphrase-protected LUKS setup with manual unlock after reboot, or consider a dedicated HSM.

**Q: How do I change the admin password?**

```bash
# Set the new password in deploy/.env
sed -i 's/^MALLARD_ADMIN_PASSWORD=.*/MALLARD_ADMIN_PASSWORD=new-password-here/' deploy/.env

# Restart mallard to pick it up
docker compose -f deploy/docker-compose.production.yml restart mallard
```

**Q: Can I use a wildcard certificate?**

Yes. DNS-01 challenge (which this setup uses) supports wildcards. Change your domain to `*.example.com` in the `Caddyfile` and the certificate will cover all subdomains.

**Q: How do I run Mallard Metrics on a private/internal network with no public IP?**

Since we use the DNS-01 challenge, the server does not need to be reachable on port 80 from the internet. Any server that can make outbound HTTPS requests to Cloudflare's API can get a certificate — including servers on private VPNs, home labs, and internal networks.

**Q: What happens to data if the LUKS container runs out of space?**

Mallard will return errors on write (DuckDB INSERT and Parquet COPY TO will fail). Flush failures are counted in the `mallard_flush_failures_total` Prometheus metric. In-memory buffered events are preserved and retried. To prevent this, monitor disk usage and enable `MALLARD_RETENTION_DAYS` to automatically delete old partitions.

**Q: Can I enable Let's Encrypt staging to test without hitting rate limits?**

Yes. In `deploy/Caddyfile`, uncomment:
```
acme_ca https://acme-staging-v02.api.letsencrypt.org/directory
```
Your browser will show a certificate warning (staging certs aren't trusted), but you can verify the issuance flow. Remove the line and `docker compose restart caddy` to switch back to production.

**Q: How do I integrate this with Grafana or another dashboard?**

Use the Prometheus `/metrics` endpoint as a data source. For detailed analytics data, the JSON export endpoint (`GET /api/stats/export?format=json`) produces daily rollups that can be ingested into any TSDB.

---

## Index

| Term | Section |
|---|---|
| A record | [Domain and DNS](#domain-and-dns) |
| ACME | [Caddy and TLS](#caddy-and-tls) |
| Admin password | [FAQ — change password](#frequently-asked-questions) |
| Backup | [Backup and restore](#backup-and-restore) |
| Caddy | [Architecture](#architecture), [Caddy and TLS](#caddy-and-tls) |
| Certificate renewal | [Certificate renewal](#certificate-renewal) |
| Cloudflare API token | [Cloudflare API token](#cloudflare-api-token) |
| Configuration | [Configuration Reference](#configuration-reference) |
| crypttab | [After reboot](#after-reboot) |
| Dashboard embed | [Embedding the Dashboard in Docs](#embedding-the-dashboard-in-docs) |
| DNS-01 challenge | [Cloudflare DNS challenge](#cloudflare-dns-challenge) |
| Docker Compose | [One-Command Deployment](#one-command-deployment) |
| fail2ban | [SSH hardening](#ssh-hardening) |
| Firewall | [Firewall (UFW)](#firewall-ufw) |
| GeoIP | [GeoIP setup](#geoip-setup) |
| Health check | [Monitoring](#monitoring) |
| HTTP/3 QUIC | [Architecture](#architecture) |
| Kernel hardening | [Kernel parameters](#kernel-parameters) |
| LUKS encryption | [LUKS Encrypted Volume](#luks-encrypted-volume) |
| Logging | [View logs](#view-logs) |
| Metrics (Prometheus) | [Monitoring](#monitoring) |
| Multi-site | [FAQ — multiple sites](#frequently-asked-questions) |
| Resize volume | [Resizing the volume](#resizing-the-volume) |
| setup.sh | [What setup.sh Does](#what-setupsh-does) |
| SSH key | [SSH key access](#ssh-key-access) |
| TLS | [Caddy and TLS](#caddy-and-tls) |
| Tracking script | [Adding the Tracking Script](#adding-the-tracking-script) |
| UFW | [Firewall (UFW)](#firewall-ufw) |
| Updates | [Update Mallard Metrics](#update-mallard-metrics) |
| vps-audit | [vps-audit integration](#vps-audit-integration) |
| Wildcard certificate | [FAQ — wildcard certificate](#frequently-asked-questions) |
