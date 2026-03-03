#!/usr/bin/env bash
# =============================================================================
# Mallard Metrics — VPS Production Setup Script
# =============================================================================
# Tested on: Ubuntu 22.04 LTS, Ubuntu 24.04 LTS, Debian 12
#
# What this script does (in order):
#   1.  Validates prerequisites and OS
#   2.  Updates system packages and installs security-critical tooling
#   3.  Hardens SSH (disables root login, password auth, sets key-only access)
#   4.  Configures UFW firewall (22/tcp, 80/tcp, 443/tcp, 443/udp)
#   5.  Applies kernel hardening sysctl settings
#   6.  Installs Docker CE and Docker Compose plugin
#   7.  Creates and mounts a LUKS-encrypted data volume for analytics data
#   8.  Downloads and runs vps-audit (github.com/tomtom215/vps-audit)
#   9.  Creates the deploy/.env file from .env.example (interactive prompts)
#   10. Builds and starts the production Docker Compose stack
#   11. Installs weekly vps-audit and daily backup cron jobs
#   12. Prints a post-setup summary and checklist
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/tomtom215/mallardmetrics/main/deploy/setup.sh \
#     | sudo bash
#
# Or clone the repo first (recommended — lets you inspect the script):
#   git clone https://github.com/tomtom215/mallardmetrics.git
#   cd mallardmetrics/deploy
#   sudo bash setup.sh
#
# Environment variables you can pre-set to skip interactive prompts:
#   MM_DOMAIN, MM_EMAIL, MM_CF_TOKEN, MM_ADMIN_PASS, MM_SECRET, MM_METRICS_TOKEN
# =============================================================================

set -euo pipefail
IFS=$'\n\t'

# ── Colour helpers ────────────────────────────────────────────────────────────
RED='\033[0;31m'; YELLOW='\033[0;33m'; GREEN='\033[0;32m'
CYAN='\033[0;36m'; BOLD='\033[1m'; RESET='\033[0m'

info()    { echo -e "${CYAN}[INFO]${RESET}  $*"; }
ok()      { echo -e "${GREEN}[OK]${RESET}    $*"; }
warn()    { echo -e "${YELLOW}[WARN]${RESET}  $*"; }
err()     { echo -e "${RED}[ERROR]${RESET} $*" >&2; }
die()     { err "$*"; exit 1; }
section() { echo -e "\n${BOLD}${CYAN}══════════════════════════════════════════════════${RESET}"; \
            echo -e "${BOLD}${CYAN}  $*${RESET}"; \
            echo -e "${BOLD}${CYAN}══════════════════════════════════════════════════${RESET}"; }
ask()     { echo -en "${YELLOW}[INPUT]${RESET} $* "; }

# ── Constants ─────────────────────────────────────────────────────────────────
MALLARD_DIR="/srv/mallard"
DATA_DIR="${MALLARD_DIR}/data"
LUKS_IMAGE="${MALLARD_DIR}/data.img"
LUKS_NAME="mallard-data"
LUKS_KEYFILE="/etc/mallard-data.key"
LUKS_SIZE_GB=20                          # Initial encrypted container size (GB)
DEPLOY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "${DEPLOY_DIR}")"
VPS_AUDIT_BIN="/usr/local/bin/vps-audit"
VPS_AUDIT_URL="https://raw.githubusercontent.com/tomtom215/vps-audit/main/vps-audit.sh"

# ── 0. Prerequisite checks ────────────────────────────────────────────────────
section "Prerequisite checks"

[[ $EUID -eq 0 ]] || die "This script must be run as root (use sudo)."

# OS detection
if [[ -f /etc/os-release ]]; then
    # shellcheck source=/dev/null
    source /etc/os-release
    OS_ID="${ID:-unknown}"
    OS_VERSION="${VERSION_ID:-0}"
else
    die "Cannot detect OS (/etc/os-release missing)."
fi

case "$OS_ID" in
    ubuntu)
        [[ "${OS_VERSION%%.*}" -ge 22 ]] || \
            warn "Ubuntu < 22.04 is untested. Proceeding anyway."
        ;;
    debian)
        [[ "${OS_VERSION%%.*}" -ge 11 ]] || \
            warn "Debian < 11 is untested. Proceeding anyway."
        ;;
    *)
        warn "OS '${OS_ID}' is untested. This script targets Ubuntu/Debian."
        ask "Continue anyway? [y/N]"; read -r ans
        [[ "$ans" =~ ^[Yy]$ ]] || die "Aborted by user."
        ;;
esac

ok "OS: ${PRETTY_NAME:-$OS_ID $OS_VERSION}"

# Check we have an SSH authorised key before we harden SSH
if [[ ! -f /root/.ssh/authorized_keys ]] && [[ ! -f /home/*/.ssh/authorized_keys ]] 2>/dev/null; then
    warn "No authorized_keys file found. SSH hardening will disable password auth."
    warn "Make sure you have an SSH key configured BEFORE continuing, or you will"
    warn "be locked out of your server."
    ask "I have SSH key access configured. Continue? [y/N]"; read -r ans
    [[ "$ans" =~ ^[Yy]$ ]] || die "Aborted. Add your SSH public key first."
fi

ok "Prerequisite checks passed."

# ── 1. System update ──────────────────────────────────────────────────────────
section "System update"

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get upgrade -y -qq
apt-get install -y -qq \
    ca-certificates curl gnupg lsb-release \
    ufw fail2ban unattended-upgrades \
    cryptsetup-bin e2fsprogs \
    jq openssl git

ok "System packages updated."

# Enable automatic security updates
cat > /etc/apt/apt.conf.d/20auto-upgrades <<'EOF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
APT::Periodic::AutocleanInterval "7";
EOF
ok "Unattended security upgrades enabled."

# ── 2. SSH hardening ──────────────────────────────────────────────────────────
section "SSH hardening"

SSHD_CONFIG="/etc/ssh/sshd_config"
SSHD_BACKUP="/etc/ssh/sshd_config.backup-$(date +%Y%m%d%H%M%S)"
cp "$SSHD_CONFIG" "$SSHD_BACKUP"
info "sshd_config backed up to $SSHD_BACKUP"

# Apply hardened settings via a drop-in file (non-destructive)
cat > /etc/ssh/sshd_config.d/99-mallard-hardening.conf <<'EOF'
# Mallard Metrics — SSH hardening
PermitRootLogin no
PasswordAuthentication no
ChallengeResponseAuthentication no
PubkeyAuthentication yes
AuthorizedKeysFile .ssh/authorized_keys
MaxAuthTries 3
LoginGraceTime 30
ClientAliveInterval 300
ClientAliveCountMax 2
AllowAgentForwarding no
AllowTcpForwarding no
X11Forwarding no
PrintMotd no
PermitEmptyPasswords no
EOF

# Reload SSH (not restart — preserves current session)
systemctl reload ssh 2>/dev/null || systemctl reload sshd 2>/dev/null || true
ok "SSH hardened (root login and password auth disabled)."

# ── 3. Firewall (UFW) ─────────────────────────────────────────────────────────
section "Firewall configuration"

ufw --force reset > /dev/null
ufw default deny incoming
ufw default allow outgoing
ufw allow 22/tcp   comment "SSH"
ufw allow 80/tcp   comment "HTTP (redirect to HTTPS)"
ufw allow 443/tcp  comment "HTTPS"
ufw allow 443/udp  comment "HTTP/3 QUIC"
ufw --force enable
ok "UFW enabled: 22/tcp, 80/tcp, 443/tcp, 443/udp."

# ── 4. fail2ban ───────────────────────────────────────────────────────────────
# Protect SSH against brute-force (Mallard has its own login-rate limiter).
cat > /etc/fail2ban/jail.d/sshd-local.conf <<'EOF'
[sshd]
enabled  = true
port     = 22
filter   = sshd
logpath  = %(sshd_log)s
backend  = systemd
maxretry = 5
bantime  = 3600
findtime = 600
EOF
systemctl enable --now fail2ban > /dev/null 2>&1 || true
ok "fail2ban enabled for SSH."

# ── 5. Kernel hardening (sysctl) ──────────────────────────────────────────────
section "Kernel hardening"

cat > /etc/sysctl.d/99-mallard-hardening.conf <<'EOF'
# Network — prevent common attacks
net.ipv4.tcp_syncookies            = 1
net.ipv4.tcp_rfc1337               = 1
net.ipv4.conf.all.rp_filter        = 1
net.ipv4.conf.default.rp_filter    = 1
net.ipv4.conf.all.accept_redirects = 0
net.ipv4.conf.all.send_redirects   = 0
net.ipv4.conf.all.accept_source_route = 0
net.ipv6.conf.all.accept_redirects = 0
net.ipv4.icmp_echo_ignore_broadcasts = 1

# Restrict dmesg and ptrace to privileged users
kernel.dmesg_restrict  = 1
kernel.perf_event_paranoid = 3
kernel.unprivileged_bpf_disabled = 1
net.core.bpf_jit_harden = 2

# Disable core dumps via setuid binaries
fs.suid_dumpable = 0
EOF

sysctl --system > /dev/null 2>&1 || true
ok "Kernel hardening settings applied."

# ── 6. Install Docker CE ──────────────────────────────────────────────────────
section "Docker CE installation"

if command -v docker &> /dev/null; then
    DOCKER_VERSION="$(docker --version)"
    ok "Docker already installed: $DOCKER_VERSION"
else
    info "Installing Docker CE from the official repository..."
    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL "https://download.docker.com/linux/${OS_ID}/gpg" \
        | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
    chmod a+r /etc/apt/keyrings/docker.gpg

    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
https://download.docker.com/linux/${OS_ID} $(lsb_release -cs) stable" \
        > /etc/apt/sources.list.d/docker.list

    apt-get update -qq
    apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

    systemctl enable --now docker
    ok "Docker CE installed and started."
fi

# Add the invoking user to docker group (if run with sudo, add the sudoer)
SUDO_USER="${SUDO_USER:-}"
if [[ -n "$SUDO_USER" ]] && id "$SUDO_USER" &>/dev/null; then
    usermod -aG docker "$SUDO_USER"
    ok "Added $SUDO_USER to docker group (re-login to take effect)."
fi

# ── 7. LUKS-encrypted data volume ────────────────────────────────────────────
section "LUKS encrypted data volume"

mkdir -p "${MALLARD_DIR}"

if mountpoint -q "$DATA_DIR"; then
    ok "LUKS volume already mounted at $DATA_DIR — skipping."
else
    if [[ -f "$LUKS_IMAGE" ]]; then
        info "Existing LUKS image found at $LUKS_IMAGE."
    else
        info "Creating ${LUKS_SIZE_GB}GB encrypted image at $LUKS_IMAGE ..."
        info "(This may take a minute — writing zeros to allocate space)"
        fallocate -l "${LUKS_SIZE_GB}G" "$LUKS_IMAGE"
        ok "Image file created."

        # Generate a random keyfile so the volume auto-mounts on boot
        # without a passphrase prompt (the keyfile itself is root-readable only).
        info "Generating keyfile at $LUKS_KEYFILE ..."
        dd if=/dev/urandom of="$LUKS_KEYFILE" bs=512 count=4 status=none
        chmod 400 "$LUKS_KEYFILE"

        info "Formatting LUKS container (aes-xts-plain64, 512-bit key) ..."
        cryptsetup luksFormat \
            --type luks2 \
            --cipher aes-xts-plain64 \
            --key-size 512 \
            --hash sha256 \
            --iter-time 2000 \
            --batch-mode \
            --key-file "$LUKS_KEYFILE" \
            "$LUKS_IMAGE"

        ok "LUKS container formatted."
    fi

    # Open the LUKS container
    if [[ ! -b "/dev/mapper/${LUKS_NAME}" ]]; then
        cryptsetup luksOpen --key-file "$LUKS_KEYFILE" "$LUKS_IMAGE" "$LUKS_NAME"
    fi

    # Format if brand new
    if ! blkid "/dev/mapper/${LUKS_NAME}" | grep -q ext4; then
        info "Formatting ext4 filesystem inside LUKS container ..."
        mkfs.ext4 -q -L mallard-data "/dev/mapper/${LUKS_NAME}"
    fi

    mkdir -p "$DATA_DIR"
    mount "/dev/mapper/${LUKS_NAME}" "$DATA_DIR"
    mkdir -p "${DATA_DIR}/logs"
    chmod 700 "$DATA_DIR"
    ok "LUKS volume mounted at $DATA_DIR."

    # Persist across reboots via crypttab + fstab
    if ! grep -q "${LUKS_NAME}" /etc/crypttab 2>/dev/null; then
        echo "${LUKS_NAME}  ${LUKS_IMAGE}  ${LUKS_KEYFILE}  luks,nofail" >> /etc/crypttab
    fi
    if ! grep -q "$DATA_DIR" /etc/fstab 2>/dev/null; then
        # nofail: boot continues even if the LUKS device fails to appear.
        # Systemd automatically orders the mount after the cryptsetup service
        # via the device unit dependency — no x-systemd.requires needed.
        # (x-systemd.requires with a literal hyphen would not match the
        #  systemd-escaped unit name and would be silently ignored.)
        echo "/dev/mapper/${LUKS_NAME}  ${DATA_DIR}  ext4  defaults,nofail  0 0" >> /etc/fstab
    fi
    ok "LUKS auto-mount configured via /etc/crypttab and /etc/fstab."
fi

# ── 8. vps-audit ─────────────────────────────────────────────────────────────
section "Security audit (vps-audit)"

info "Downloading vps-audit from github.com/tomtom215/vps-audit ..."
curl -fsSL "$VPS_AUDIT_URL" -o "$VPS_AUDIT_BIN"
chmod +x "$VPS_AUDIT_BIN"

AUDIT_LOG="${MALLARD_DIR}/vps-audit-initial-$(date +%Y%m%d).log"
info "Running initial security audit (log: $AUDIT_LOG) ..."
"$VPS_AUDIT_BIN" 2>&1 | tee "$AUDIT_LOG" || true

ok "vps-audit complete. Review $AUDIT_LOG for WARN/FAIL items."

# Weekly re-audit cron
cat > /etc/cron.d/vps-audit-weekly <<EOF
# Weekly VPS security re-audit — results logged for review
0 3 * * 0 root ${VPS_AUDIT_BIN} >> /var/log/vps-audit.log 2>&1
EOF
ok "Weekly vps-audit cron installed (/etc/cron.d/vps-audit-weekly)."

# ── 9. Configure .env ─────────────────────────────────────────────────────────
section "Mallard Metrics configuration"

ENV_FILE="${DEPLOY_DIR}/.env"

if [[ -f "$ENV_FILE" ]]; then
    ok ".env already exists at $ENV_FILE — skipping interactive setup."
    warn "Edit $ENV_FILE manually if any values need updating."
else
    cp "${DEPLOY_DIR}/.env.example" "$ENV_FILE"
    chmod 600 "$ENV_FILE"

    # Helper: ask for value, update .env
    set_env() {
        local key="$1" prompt="$2" default="${3:-}"
        local val=""
        if [[ -n "${!key:-}" ]]; then
            val="${!key}"
            ok "Using pre-set $key from environment."
        else
            if [[ -n "$default" ]]; then
                ask "$prompt [${default}]:"
            else
                ask "$prompt:"
            fi
            read -r val
            [[ -z "$val" ]] && val="$default"
        fi
        # Escape / for sed
        val_escaped="${val//\//\\/}"
        sed -i "s|^${key}=.*|${key}=${val_escaped}|" "$ENV_FILE"
    }

    echo ""
    info "Fill in the required configuration values."
    info "Leave blank for defaults where shown in brackets."
    echo ""

    set_env "DOMAIN"                "Analytics domain (e.g. analytics.example.com)"
    set_env "ACME_EMAIL"            "Let's Encrypt email"
    set_env "CLOUDFLARE_API_TOKEN"  "Cloudflare API token (Zone:DNS:Edit)"

    info "Generating strong secrets automatically ..."
    GENERATED_SECRET="$(openssl rand -base64 48)"
    GENERATED_PASSWORD="$(openssl rand -base64 24 | tr -d '=+/' | head -c 32)"
    GENERATED_METRICS_TOKEN="$(openssl rand -hex 32)"

    sed -i "s|^MALLARD_SECRET=.*|MALLARD_SECRET=${GENERATED_SECRET}|" "$ENV_FILE"
    sed -i "s|^MALLARD_ADMIN_PASSWORD=.*|MALLARD_ADMIN_PASSWORD=${GENERATED_PASSWORD}|" "$ENV_FILE"
    sed -i "s|^MALLARD_METRICS_TOKEN=.*|MALLARD_METRICS_TOKEN=${GENERATED_METRICS_TOKEN}|" "$ENV_FILE"

    ok ".env created at $ENV_FILE"
    echo ""
    echo -e "${BOLD}Generated credentials (save these securely now):${RESET}"
    echo -e "  Admin password: ${GREEN}${GENERATED_PASSWORD}${RESET}"
    echo -e "  Metrics token:  ${GREEN}${GENERATED_METRICS_TOKEN}${RESET}"
    echo ""
    warn "These are stored in $ENV_FILE — make sure only root can read it."
fi

# ── 10. Build and start the stack ─────────────────────────────────────────────
section "Building and starting Mallard Metrics"

cd "${REPO_ROOT}"
info "Building Docker images (this takes 5–15 minutes on first run) ..."
docker compose -f deploy/docker-compose.production.yml build --no-cache

info "Starting services ..."
docker compose -f deploy/docker-compose.production.yml --env-file deploy/.env up -d

# Wait for Caddy to obtain certificate
info "Waiting 30 seconds for Caddy to obtain TLS certificate ..."
sleep 30

# Quick health check
DOMAIN_VAL="$(grep '^DOMAIN=' "${ENV_FILE}" | cut -d= -f2)"
HTTP_CODE="$(curl -sSo /dev/null -w '%{http_code}' --max-time 10 \
    "https://${DOMAIN_VAL}/health/ready" 2>/dev/null || echo "000")"

if [[ "$HTTP_CODE" == "200" ]]; then
    ok "Health check passed — Mallard Metrics is live at https://${DOMAIN_VAL}"
else
    warn "Health check returned HTTP ${HTTP_CODE}. Caddy may still be obtaining the certificate."
    info "Check logs with: docker compose -f deploy/docker-compose.production.yml logs -f"
fi

# ── 11. Maintenance cron jobs ─────────────────────────────────────────────────
section "Maintenance cron jobs"

# Daily backup: copy DuckDB file and newest Parquet partitions to backup dir
cat > /etc/cron.d/mallard-backup <<EOF
# Daily Mallard Metrics backup (DuckDB file + recent Parquet)
30 2 * * * root rsync -a --delete ${DATA_DIR}/ ${MALLARD_DIR}/backup/ 2>&1 | logger -t mallard-backup
EOF
mkdir -p "${MALLARD_DIR}/backup"
ok "Daily backup cron installed (rsync to ${MALLARD_DIR}/backup)."

# ── 12. Post-setup summary ────────────────────────────────────────────────────
section "Setup complete"

DOMAIN_VAL="$(grep '^DOMAIN=' "${ENV_FILE}" | cut -d= -f2 || echo '<your-domain>')"

echo ""
echo -e "${BOLD}${GREEN}Mallard Metrics is deployed!${RESET}"
echo ""
echo -e "  Dashboard:    ${CYAN}https://${DOMAIN_VAL}${RESET}"
echo -e "  Health:       ${CYAN}https://${DOMAIN_VAL}/health/detailed${RESET}"
echo -e "  Metrics:      ${CYAN}https://${DOMAIN_VAL}/metrics${RESET}  (bearer token required)"
echo ""
echo -e "${BOLD}Post-deployment checklist:${RESET}"
echo "  [ ] Verify dashboard loads at https://${DOMAIN_VAL}"
echo "  [ ] Log in with the generated admin password"
echo "  [ ] Add the tracking script to your site(s)"
echo "  [ ] Add GeoIP database if country analytics needed:"
echo "        ${DATA_DIR}/GeoLite2-City.mmdb"
echo "  [ ] Review vps-audit output: ${MALLARD_DIR}/vps-audit-initial-$(date +%Y%m%d).log"
echo "  [ ] Set up external monitoring (UptimeRobot, Better Uptime, etc.)"
echo ""
echo -e "${BOLD}Useful commands:${RESET}"
echo "  docker compose -f ${REPO_ROOT}/deploy/docker-compose.production.yml logs -f"
echo "  docker compose -f ${REPO_ROOT}/deploy/docker-compose.production.yml ps"
echo "  docker compose -f ${REPO_ROOT}/deploy/docker-compose.production.yml restart mallard"
echo "  ${VPS_AUDIT_BIN}     # Re-run security audit"
echo ""
