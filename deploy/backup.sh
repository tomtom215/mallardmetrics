#!/usr/bin/env bash
# =============================================================================
# Mallard Metrics — consistent offline backup of the data directory
# =============================================================================
#
# Why this script stops the container
# -----------------------------------
# The data directory holds a live DuckDB database (mallard.duckdb plus its
# write-ahead log) alongside immutable Parquet partitions. DuckDB keeps the file
# open, caches pages and writes the WAL independently of the main file, so
# copying it while the server runs (what a plain `rsync` or `cp` does) can
# capture a database file and a WAL that do not belong together. Such a copy
# often fails to open, or opens with the most recent writes missing, and you
# find out when you restore it. The Parquet files are written atomically and
# are safe to copy live; the DuckDB files are not.
#
# So the only consistent copies available without a change to Mallard are:
#   * a copy taken while the process is stopped (this script), or
#   * an in-process export (not available yet; see "Online backup" below).
#
# What it does
#   1. takes an exclusive lock so two runs never overlap;
#   2. stops the mallard container (graceful: Mallard flushes its buffer and
#      closes DuckDB, which checkpoints the WAL);
#   3. copies the data directory into <dest>/<UTC timestamp>/ with rsync,
#      hard-linking every file that is unchanged since the previous snapshot,
#      so keeping many snapshots costs little extra disk;
#   4. checks that the copy matches the source;
#   5. ALWAYS starts mallard again, even when a step above failed;
#   6. keeps the newest N snapshots and deletes the rest.
#
# Cost: Mallard is unavailable for the duration of steps 2-4 (seconds for a
# small database, longer for a large one). Events the tracker sends in that
# window are lost; the tracker does not retry. Schedule it for your quietest
# hour. Caddy keeps running and answers 502 meanwhile.
#
# Usage
#   deploy/backup.sh [--dest DIR] [--keep N] [--compose-file FILE]
#
# Environment (flags win)
#   MALLARD_DATA_HOST_DIR   data directory on the host   (/srv/mallard/data)
#   MALLARD_BACKUP_DIR      where snapshots are written  (/srv/mallard/backups)
#   MALLARD_BACKUP_KEEP     snapshots to keep            (14)
#   MALLARD_COMPOSE_FILE    compose file                 (deploy/docker-compose.production.yml)
#
# Put the backup directory on a DIFFERENT disk or, better, a different machine
# (mount a remote share there, or sync <dest> off-site afterwards). A snapshot
# beside the data protects against corruption, not against losing the disk. The
# snapshots contain the admin password hash and API-key hashes (admin.json,
# api_keys.json) and the visitor-ID secret (.secret), so they are written with
# mode 700 and must be protected like the data directory itself.
#
# Restore
#   docker compose -f deploy/docker-compose.production.yml stop mallard
#   sudo rsync -a --delete /srv/mallard/backups/<snapshot>/ /srv/mallard/data/
#   sudo chown -R 65532:65532 /srv/mallard/data
#   docker compose -f deploy/docker-compose.production.yml start mallard
#
# Online backup (requested, not yet implemented)
#   An in-process `EXPORT DATABASE` / `COPY FROM DATABASE` triggered through an
#   authenticated admin endpoint would remove the downtime. Until Mallard has
#   one, this script is the supported method.
# =============================================================================

set -euo pipefail
umask 077

DEPLOY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DATA_DIR="${MALLARD_DATA_HOST_DIR:-/srv/mallard/data}"
DEST="${MALLARD_BACKUP_DIR:-/srv/mallard/backups}"
KEEP="${MALLARD_BACKUP_KEEP:-14}"
COMPOSE_FILE="${MALLARD_COMPOSE_FILE:-${DEPLOY_DIR}/docker-compose.production.yml}"
SERVICE="mallard"
STOP_TIMEOUT=60          # seconds Docker waits for a graceful shutdown
START_TIMEOUT=180        # seconds to wait for the container to report healthy

log() { printf '%s mallard-backup: %s\n' "$(date -u +%FT%TZ)" "$*"; }
die() { log "ERROR: $*" >&2; exit 1; }

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dest)         DEST="${2:?--dest needs a directory}"; shift 2 ;;
    --keep)         KEEP="${2:?--keep needs a number}"; shift 2 ;;
    --compose-file) COMPOSE_FILE="${2:?--compose-file needs a path}"; shift 2 ;;
    -h|--help)      usage; exit 0 ;;
    *)              die "unknown argument: $1 (try --help)" ;;
  esac
done

[[ "$KEEP" =~ ^[0-9]+$ ]] && [ "$KEEP" -ge 1 ] || die "--keep must be a positive integer"
[ -d "$DATA_DIR" ] || die "data directory $DATA_DIR does not exist"
[ -f "$COMPOSE_FILE" ] || die "compose file $COMPOSE_FILE not found"
command -v rsync  >/dev/null || die "rsync is required (apt-get install rsync)"
command -v docker >/dev/null || die "docker is required"

# Refuse to snapshot an empty directory: with the encrypted volume unmounted the
# path is an empty directory on the root filesystem, and "succeeding" would
# rotate good snapshots out in favour of nothing.
if [ -z "$(find "$DATA_DIR" -mindepth 1 -maxdepth 1 ! -name lost+found -print -quit)" ]; then
  die "$DATA_DIR is empty; is the encrypted volume mounted?"
fi

# One run at a time (cron firing while a manual run is in progress).
LOCK="${MALLARD_BACKUP_LOCK:-/run/lock/mallard-backup.lock}"
exec 9>"$LOCK" || die "cannot open lock file $LOCK"
flock -n 9 || die "another backup is already running"

compose() {
  local env_args=()
  [ -f "$(dirname "$COMPOSE_FILE")/.env" ] && env_args=(--env-file "$(dirname "$COMPOSE_FILE")/.env")
  docker compose -f "$COMPOSE_FILE" "${env_args[@]}" "$@"
}

mkdir -p "$DEST"
chmod 700 "$DEST"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
PARTIAL="${DEST}/.${STAMP}.partial"
FINAL="${DEST}/${STAMP}"

STOPPED=0
start_service() {
  [ "$STOPPED" -eq 1 ] || return 0
  log "starting $SERVICE"
  if ! compose start "$SERVICE" >/dev/null; then
    log "ERROR: could not start $SERVICE; start it by hand: docker compose -f $COMPOSE_FILE start $SERVICE" >&2
    return 1
  fi
  STOPPED=0
  local cid status waited=0
  cid="$(compose ps -q "$SERVICE")"
  while [ "$waited" -lt "$START_TIMEOUT" ]; do
    status="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$cid" 2>/dev/null || true)"
    case "$status" in
      healthy|running) log "$SERVICE is $status"; return 0 ;;
    esac
    sleep 3; waited=$((waited + 3))
  done
  log "WARNING: $SERVICE did not report healthy within ${START_TIMEOUT}s (last status: ${status:-unknown})" >&2
  return 1
}

cleanup() {
  local rc=$?
  start_service || rc=1
  # A failed run must not leave a half-written snapshot that looks complete.
  [ -d "$PARTIAL" ] && rm -rf "$PARTIAL"
  exit "$rc"
}
trap cleanup EXIT

# Most recent complete snapshot, used as the hard-link base.
PREVIOUS="$(find "$DEST" -mindepth 1 -maxdepth 1 -type d -name '[0-9]*T[0-9]*Z' | sort | tail -n 1)"

log "stopping $SERVICE (graceful, up to ${STOP_TIMEOUT}s)"
# Set before the call: if `stop` fails half way the container may be down, and
# the exit trap must still try to start it.
STOPPED=1
compose stop -t "$STOP_TIMEOUT" "$SERVICE" >/dev/null

log "copying $DATA_DIR -> $FINAL${PREVIOUS:+ (hard-linking unchanged files from $PREVIOUS)}"
rsync -a --delete ${PREVIOUS:+--link-dest="$PREVIOUS"} "${DATA_DIR%/}/" "$PARTIAL/"

# A second, itemized dry run reports anything that still differs in size or
# mtime. It must print nothing.
DIFF="$(rsync -an --delete --itemize-changes "${DATA_DIR%/}/" "$PARTIAL/" | grep -v '^\.d' || true)"
if [ -n "$DIFF" ]; then
  printf '%s\n' "$DIFF" | head -20 >&2
  die "the copy does not match the source; keeping the previous snapshots"
fi

mv "$PARTIAL" "$FINAL"
ln -sfn "$FINAL" "${DEST}/latest"
log "snapshot complete: $FINAL"
log "snapshot size: $(du -sh "$FINAL" | cut -f1) (files unchanged since the previous snapshot share its disk space)"

# Bring the service back before pruning: deleting old snapshots can be slow.
start_service

# Keep the newest $KEEP complete snapshots.
mapfile -t SNAPSHOTS < <(find "$DEST" -mindepth 1 -maxdepth 1 -type d -name '[0-9]*T[0-9]*Z' | sort)
if [ "${#SNAPSHOTS[@]}" -gt "$KEEP" ]; then
  for old in "${SNAPSHOTS[@]:0:${#SNAPSHOTS[@]}-KEEP}"; do
    log "pruning $old"
    rm -rf "$old"
  done
fi
log "done"
