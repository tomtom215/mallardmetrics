# Backup and Restore

## What needs backing up

Everything is under the data directory (`MALLARD_DATA_DIR`, `/data` in the
image, `/srv/mallard/data` on the production VPS layout):

| Path | Contains | Lose it and... |
|---|---|---|
| `events/` | All flushed analytics data (Parquet) | the history is gone |
| `mallard.duckdb` (+ `.wal`) | The hot table (rows not yet moved to Parquet) and the catalog | rows not yet in Parquet are gone; `events/` still works on its own |
| `.secret` | Visitor-ID secret, when `MALLARD_SECRET` is unset | every visitor gets a new ID (see [Privacy and GDPR](privacy-gdpr.md#salt-rotation)) |
| `admin.json` | Admin password hash set through setup | the instance is **unconfigured and claimable** until setup is run again |
| `api_keys.json` | API key hashes | every API key stops working |
| `extensions/` | The `behavioral` extension | re-downloaded on start, or seed it again on an isolated network |

A backup therefore contains password and key hashes and the secret: protect it
like the live directory.

## Do not copy a live data directory

DuckDB keeps `mallard.duckdb` open, caches pages and writes the WAL separately.
A plain `cp` or `rsync` of a running instance can capture a database file and a
WAL that do not belong together; the copy often fails to open, or opens without
the most recent writes, and you find out when you restore. Copying `events/`
while a compaction runs can also capture a merged file together with the files
it replaced. The only consistent copy is one taken while the process is stopped.
The memory buffer is not part of any backup: events not flushed when the
process stops are written by the shutdown flush, so stop gracefully (`SIGTERM`).

## What `deploy/backup.sh` does

For the production Docker Compose layout. `setup.sh` installs it as a daily cron
job at 02:30 (`/etc/cron.d/mallard-backup`, output to syslog under
`mallard-backup`). It does, in order:

1. Takes an exclusive `flock` so two runs never overlap.
2. Refuses to run if the data directory is missing or **empty** (for example the
   encrypted volume is not mounted), so a bad run cannot rotate good snapshots
   out in favour of nothing.
3. Stops the `mallard` container gracefully (up to 60 s), which flushes the
   buffer and checkpoints the WAL.
4. Copies the data directory to `<dest>/<UTC timestamp>/` with `rsync`,
   hard-linking every file unchanged since the previous snapshot, so many
   snapshots cost little disk.
5. Runs a second itemised `rsync` dry run and fails if anything differs.
6. **Always** starts the container again, even if a step failed, and waits up to
   180 s for it to report healthy.
7. Updates the `latest` symlink, then keeps the newest N snapshots and deletes
   the rest.

```bash
deploy/backup.sh [--dest DIR] [--keep N] [--compose-file FILE]
# environment: MALLARD_DATA_HOST_DIR (/srv/mallard/data), MALLARD_BACKUP_DIR
# (/srv/mallard/backups), MALLARD_BACKUP_KEEP (14), MALLARD_COMPOSE_FILE
```

Snapshots are written with mode 700.

### Limits

- **Downtime.** Mallard is stopped for the whole copy: seconds for a small
  database, longer for a large one. Events the tracker sends in that window are
  lost (the tracker does not retry); Caddy answers `502`. Schedule it for your
  quietest hour.
- **Same machine by default.** `/srv/mallard/backups` is usually on the same
  disk as the data: it protects against corruption and mistakes, not against
  losing the disk. Put the destination on another disk, or sync it off the
  machine afterwards (`rsync -a --delete <dest>/latest/ backup-host:/path/`).
- **Hard links** share data between snapshots: deleting one does not damage
  another, but a copy of the directory to a filesystem without hard-link support
  expands them.
- **Docker Compose only.** It runs `docker compose stop/start mallard`. For the
  systemd unit, stop the service (`systemctl stop mallard-metrics`), copy
  `/var/lib/mallard-metrics`, start it again. On Fly.io, volume snapshots are the
  mechanism; they are taken without stopping the machine, so whether a snapshot is
  consistent for DuckDB is **not verified here** (see
  [Fly.io](deploy-flyio.md#backups)).
- It needs `rsync`, `docker` and `flock` on the host.

## Restore

```bash
docker compose -f deploy/docker-compose.production.yml stop mallard
sudo rsync -a --delete /srv/mallard/backups/<snapshot>/ /srv/mallard/data/
sudo chown -R 65532:65532 /srv/mallard/data
docker compose -f deploy/docker-compose.production.yml start mallard
```

The container runs as UID 65532 and cannot write a root-owned directory, which
shows up as a startup failure naming `/data`. `--delete` makes the directory
match the snapshot exactly, which is what you want for a restore and the reason
to be careful with the path.

Then check:

```bash
curl -s https://analytics.example.com/health/ready            # ready
curl -s https://analytics.example.com/health/detailed          # "cold_tier": "healthy"
```

and compare a known report (visitors for a past week) with one you recorded
before. Startup recovery runs automatically and logs `Checked for interrupted
flushes` (see [Troubleshooting](troubleshooting.md#restart-and-recovery)).

A restore returns the instance to the snapshot's moment: events since then are
gone, and dashboard sessions are not part of any backup (everyone logs in again).

### Restoring only the analytics data

`events/` is self-contained. If `mallard.duckdb` is lost or unusable, move it
aside and start with the restored `events/`: history is intact, rows that lived
only in the hot table are lost, and `admin.json`/`api_keys.json`/`.secret` must
be restored separately or recreated (a new `.secret` means new visitor IDs).

## Test your restore

A backup you have not restored is a hope. After the first backup, restore it into
a scratch directory on another port and open the dashboard:

```bash
rsync -a /srv/mallard/backups/latest/ /tmp/restore-test/
MALLARD_DATA_DIR=/tmp/restore-test MALLARD_PORT=18000 MALLARD_HOST=127.0.0.1 \
  MALLARD_SECRET=<your secret> mallard-metrics
```
