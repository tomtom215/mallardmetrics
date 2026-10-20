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
#   8.  Optionally downloads and runs vps-audit (opt-in: MM_RUN_VPS_AUDIT=1)
#   9.  Creates the deploy/.env file from .env.example (interactive prompts)
#   10. Builds (or pulls) the images, seeds the DuckDB `behavioral` extension
#       onto the data volume, and starts the production Docker Compose stack
#   11. Installs a daily consistent backup (deploy/backup.sh) as a cron job
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
#   MM_IMAGE          pull this image (e.g. ghcr.io/tomtom215/mallardmetrics:<version>)
#                     instead of compiling DuckDB on the server
#   MM_RUN_VPS_AUDIT  set to 1 to download and run vps-audit (see step 8)
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
BACKUP_DIR="${MALLARD_DIR}/backups"
# The container runs as this uid:gid (Dockerfile USER, compose `user:`); the
# data directory must belong to it or Mallard cannot write its database.
MALLARD_UID=65532
LUKS_IMAGE="${MALLARD_DIR}/data.img"
LUKS_NAME="mallard-data"
LUKS_KEYFILE="/etc/mallard-data.key"
LUKS_SIZE_GB=20                          # Initial encrypted container size (GB)
DEPLOY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "${DEPLOY_DIR}")"
VPS_AUDIT_BIN="/usr/local/bin/vps-audit"
COMPOSE=(docker compose -f deploy/docker-compose.production.yml)
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

# Hardening below disables root login and password auth, so a non-root user
# with a key must already exist or the server becomes unreachable. (The test
# is a loop because a glob inside `[[ -f ]]` is never expanded.)
nonroot_key=0
for keyfile in /home/*/.ssh/authorized_keys; do
    [[ -s "$keyfile" ]] && nonroot_key=1
done
if [[ "$nonroot_key" -eq 0 ]]; then
    warn "No non-root user has an SSH authorized_keys file."
    warn "SSH hardening disables root login AND password auth: without a key for"
    warn "a non-root sudo user you will be locked out of this server."
    ask "I have SSH key access for a non-root user configured. Continue? [y/N]"; read -r ans
    [[ "$ans" =~ ^[Yy]$ ]] || die "Aborted. Add your SSH public key to a non-root user first."
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
    jq openssl git rsync logrotate

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
KbdInteractiveAuthentication no
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

# Allow the port sshd actually listens on (it may not be 22) or the reset below
# would cut off the session that is running this script on the next connect.
SSH_PORT="$(sshd -T 2>/dev/null | awk '$1 == "port" { print $2; exit }')"
SSH_PORT="${SSH_PORT:-22}"

ufw --force reset > /dev/null
ufw default deny incoming
ufw default allow outgoing
ufw allow "${SSH_PORT}/tcp" comment "SSH"
ufw allow 80/tcp   comment "HTTP (redirect to HTTPS)"
ufw allow 443/tcp  comment "HTTPS"
ufw allow 443/udp  comment "HTTP/3 QUIC"
ufw --force enable
ok "UFW enabled: ${SSH_PORT}/tcp, 80/tcp, 443/tcp, 443/udp."

# ── 4. fail2ban ───────────────────────────────────────────────────────────────
# Protect SSH against brute-force (Mallard has its own login-rate limiter).
cat > /etc/fail2ban/jail.d/sshd-local.conf <<EOF
[sshd]
enabled  = true
port     = ${SSH_PORT}
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
    # Make the bare mountpoint immutable. If the volume ever fails to mount at
    # boot, nothing can then write into the empty directory underneath and
    # silently split the data between two filesystems.
    chattr +i "$DATA_DIR" 2>/dev/null || warn "Could not set the immutable bit on $DATA_DIR."
    mount "/dev/mapper/${LUKS_NAME}" "$DATA_DIR"
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

# The container runs as uid 65532 and its root filesystem is read-only; /data is
# the only place it can write. A root-owned mode-700 directory (what this script
# used to leave behind) makes Mallard fail at startup with "permission denied".
# Done outside the branch above so re-running the script repairs an existing
# installation.
chown "${MALLARD_UID}:${MALLARD_UID}" "$DATA_DIR"
chmod 700 "$DATA_DIR"
find "$DATA_DIR" -xdev -mindepth 1 -not -path "${DATA_DIR}/lost+found*" \
    ! -user "$MALLARD_UID" -exec chown "${MALLARD_UID}:${MALLARD_UID}" {} +
ok "$DATA_DIR is owned by uid ${MALLARD_UID}."

# Do not start Docker (and its restart: unless-stopped containers) before the
# encrypted volume is mounted, or Mallard would create a fresh database on the
# unencrypted root filesystem and the real data would look lost.
mkdir -p /etc/systemd/system/docker.service.d
cat > /etc/systemd/system/docker.service.d/mallard-data-mount.conf <<EOF
[Unit]
RequiresMountsFor=${DATA_DIR}
EOF
systemctl daemon-reload
ok "Docker now waits for ${DATA_DIR} to be mounted at boot."

# ── 8. vps-audit ─────────────────────────────────────────────────────────────
section "Security audit (vps-audit)"

if [[ "${MM_RUN_VPS_AUDIT:-0}" == "1" ]]; then
    # Opt-in: this downloads a script from a branch (not a pinned commit) and runs
    # it as root. Read it first (curl -fsSL "$VPS_AUDIT_URL" | less) and prefer to
    # pin VPS_AUDIT_URL to a commit.
    info "Downloading vps-audit from github.com/tomtom215/vps-audit ..."
    curl -fsSL "$VPS_AUDIT_URL" -o "$VPS_AUDIT_BIN"
    chmod 0755 "$VPS_AUDIT_BIN"
    info "vps-audit sha256: $(sha256sum "$VPS_AUDIT_BIN" | cut -d' ' -f1)"

    AUDIT_LOG="${MALLARD_DIR}/vps-audit-initial-$(date +%Y%m%d).log"
    info "Running initial security audit (log: $AUDIT_LOG) ..."
    "$VPS_AUDIT_BIN" 2>&1 | tee "$AUDIT_LOG" || true
    ok "vps-audit complete. Review $AUDIT_LOG for WARN/FAIL items."

    # Weekly re-audit cron
    cat > /etc/cron.d/vps-audit-weekly <<EOF
# Weekly VPS security re-audit — results logged for review
0 3 * * 0 root ${VPS_AUDIT_BIN} >> /var/log/vps-audit.log 2>&1
EOF
    cat > /etc/logrotate.d/vps-audit <<'EOF'
/var/log/vps-audit.log {
    monthly
    rotate 6
    compress
    missingok
    notifempty
}
EOF
    ok "Weekly vps-audit cron installed (/etc/cron.d/vps-audit-weekly)."
else
    info "Skipping vps-audit (set MM_RUN_VPS_AUDIT=1 to download and run it)."
fi

# ── 9. Configure .env ─────────────────────────────────────────────────────────
section "Mallard Metrics configuration"

ENV_FILE="${DEPLOY_DIR}/.env"

# Replace KEY=... in .env. The value is escaped for sed's replacement text
# (backslash, ampersand and the | delimiter) so any password or token works.
set_kv() {
    local esc
    esc="$(printf '%s' "$2" | sed -e 's/[\\&|]/\\&/g')"
    sed -i "s|^${1}=.*|${1}=${esc}|" "$ENV_FILE"
}

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
        set_kv "$key" "$val"
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

    set_kv MALLARD_SECRET "$GENERATED_SECRET"
    set_kv MALLARD_ADMIN_PASSWORD "$GENERATED_PASSWORD"
    set_kv MALLARD_METRICS_TOKEN "$GENERATED_METRICS_TOKEN"

    ok ".env created at $ENV_FILE"
    echo ""
    echo -e "${BOLD}Generated credentials (save these securely now):${RESET}"
    echo -e "  Admin password: ${GREEN}${GENERATED_PASSWORD}${RESET}"
    echo -e "  Metrics token:  ${GREEN}${GENERATED_METRICS_TOKEN}${RESET}"
    echo ""
    warn "These are stored in $ENV_FILE — make sure only root can read it."
fi

# Optional: run a released image instead of compiling DuckDB on this server.
if [[ -n "${MM_IMAGE:-}" ]]; then
    grep -q '^MALLARD_IMAGE=' "$ENV_FILE" || echo "MALLARD_IMAGE=" >> "$ENV_FILE"
    set_kv MALLARD_IMAGE "$MM_IMAGE"
    ok "Using pre-built image ${MM_IMAGE}."
fi

# ── 10. Build and start the stack ─────────────────────────────────────────────
section "Building and starting Mallard Metrics"

cd "${REPO_ROOT}"
MALLARD_IMAGE_REF="$(grep '^MALLARD_IMAGE=' "$ENV_FILE" | tail -n 1 | cut -d= -f2-)"
if [[ -n "$MALLARD_IMAGE_REF" ]]; then
    info "Pulling ${MALLARD_IMAGE_REF} and building Caddy ..."
    "${COMPOSE[@]}" pull mallard
    "${COMPOSE[@]}" build --pull caddy
else
    MALLARD_IMAGE_REF="mallard-metrics:latest"
    info "Building Docker images (compiling DuckDB takes 15+ minutes and several GB of RAM) ..."
    "${COMPOSE[@]}" build --pull
fi

# Mallard runs on an internal network with no route to the internet, so it can
# never download the DuckDB `behavioral` extension itself. Without it funnels,
# retention, sessions, sequences and flow answer 503. Seed it once from a
# throwaway container on the default (routable) network; it lands in
# ${DATA_DIR}/extensions and is loaded from disk from then on. The flags mirror
# the production container's restrictions, which also proves they work.
info "Installing the behavioral extension onto the data volume ..."
if docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges:true \
        --user "${MALLARD_UID}:${MALLARD_UID}" -v "${DATA_DIR}:/data" \
        "$MALLARD_IMAGE_REF" --install-extension; then
    ok "behavioral extension installed."
else
    warn "Could not install the behavioral extension (no outbound network, or a"
    warn "musl-based image). Funnels, retention, sessions, sequences and flow will"
    warn "answer 503 until it is: docker run --rm --user ${MALLARD_UID}:${MALLARD_UID} \\"
    warn "  -v ${DATA_DIR}:/data ${MALLARD_IMAGE_REF} --install-extension"
fi

info "Starting services ..."
"${COMPOSE[@]}" --env-file deploy/.env up -d

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

# Daily backup. deploy/backup.sh stops Mallard briefly so the DuckDB file and its
# write-ahead log are copied consistently; the previous cron job rsync-ed the
# live database, which can produce a copy that does not open. Snapshots go to a
# separate directory; point MALLARD_BACKUP_DIR at another disk or an off-site
# mount for protection against losing the host.
mkdir -p "$BACKUP_DIR"
chmod 700 "$BACKUP_DIR"
cat > /etc/cron.d/mallard-backup <<EOF
# Daily Mallard Metrics backup (brief stop, consistent copy, 14 snapshots kept)
30 2 * * * root MALLARD_DATA_HOST_DIR=${DATA_DIR} MALLARD_BACKUP_DIR=${BACKUP_DIR} ${DEPLOY_DIR}/backup.sh 2>&1 | logger -t mallard-backup
EOF
chmod 644 /etc/cron.d/mallard-backup
ok "Daily backup cron installed (${DEPLOY_DIR}/backup.sh -> ${BACKUP_DIR}, mode 700)."
warn "Backups on the same disk do not survive losing the disk: mount remote storage at"
warn "${BACKUP_DIR} or sync it off-site."

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
echo "  [ ] Check /health/detailed reports a behavioral_version (see docs/src/deploy-vps.md)"
echo "  [ ] Copy a backup off this machine and test a restore from ${BACKUP_DIR}"
echo "  [ ] Set up external monitoring (UptimeRobot, Better Uptime, etc.)"
echo ""
echo -e "${BOLD}Useful commands:${RESET}"
echo "  docker compose -f ${REPO_ROOT}/deploy/docker-compose.production.yml logs -f"
echo "  docker compose -f ${REPO_ROOT}/deploy/docker-compose.production.yml ps"
echo "  docker compose -f ${REPO_ROOT}/deploy/docker-compose.production.yml restart mallard"
echo "  ${DEPLOY_DIR}/backup.sh   # Take a consistent backup now (stops Mallard briefly)"
echo ""
