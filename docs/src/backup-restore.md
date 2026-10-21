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
| `share_links.json` | Share link hashes, labels, segments, expiry | every [share link](share-links.md) stops working |
| `segments.json` | Saved segments | the saved segments are gone (share links keep their own copy) |
| `extensions/` | The `behavioral` extension | re-downloaded on start, or seed it again on an isolated network |

A backup therefore contains password, key and share-link hashes and the secret:
protect it like the live directory.

**Restoring an old backup brings back revoked share links**, because the file it
restores predates the revocation. After any restore, list the links
(`GET /api/share-links`) and revoke what should be dead.

## Do not copy a live data directory

DuckDB keeps `mallard.duckdb` open, caches pages and writes the WAL separately.
A plain `cp` or `rsync` of a running instance can capture a database file and a
WAL that do not belong together; the copy often fails to open, or opens without
the most recent writes, and you find out when you restore. Copying `events/`
while a compaction runs can also capture a merged file together with the files
it replaced. The only consistent copies are one taken while the process is
stopped and one taken by the server itself with the online backup below.
The memory buffer is not part of any backup: events not flushed when the
process stops are written by the shutdown flush, so stop gracefully (`SIGTERM`).

## Online backup (no downtime)

`POST /api/admin/backup` (admin scope) makes a consistent snapshot while the
server keeps serving:

```bash
curl -X POST -H "Authorization: Bearer $ADMIN_KEY" \
  "https://analytics.example.com/api/admin/backup?name=before-upgrade"
# {"status":"created","name":"before-upgrade","path":"/data/backups/before-upgrade",
#  "parquet_files":212,"parquet_bytes":48123456,"hot_rows":318,"state_files":3}
```

(The figures are an illustration; `state_files` is how many of `.secret`,
`admin.json`, `api_keys.json`, `share_links.json` and `segments.json` existed.)

It writes `<data_dir>/backups/<name>/` (default name `mallard-<UTC timestamp>`)
holding `mallard.duckdb`, `events/`, whichever of `.secret`, `admin.json`,
`api_keys.json`, `share_links.json` and `segments.json` exist, and a `MANIFEST.json`. The directory and the secrets in it
are owner-only. `extensions/` is left out because it is downloaded again on start.

How it stays consistent: the server takes the lock that a flush takes, so no
flush, compaction, retention run or erasure can change either tier, and queries
pause, while it checkpoints and copies the hot table and **hard-links** the
Parquet files. A published Parquet file is never modified in place, so a link keeps
exactly the bytes that existed at that moment and costs almost no disk. Queries
and flushes wait only for that copy, which is short unless the hot table is
large; ingestion keeps buffering throughout.

Rules and limits:

- `name` is only a name (letters, digits, `-`, `_`, `.`; not starting with a dot
  or ending in `.partial`, the suffix an unfinished backup is written under).
  The endpoint cannot write anywhere but `<data_dir>/backups`. An existing name is
  a `409`; backups are never overwritten. A backup is written to `<name>.partial`
  and renamed last, so a directory without that suffix is complete.
- **It is on the same disk.** Hard links share blocks with the live data, so this
  protects against mistakes and corruption, not against losing the disk. Copy the
  directory to another machine afterwards (`rsync -a`, which un-links on a
  different filesystem), and delete old backups yourself: nothing prunes them.
- **A backup keeps what you later erase.** `DELETE /api/gdpr/erase` removes data
  from the live tiers only. Backups hold their own copy (the Parquet files are
  hard links to the same bytes, which survive the live name being removed), so an
  erased visitor's events remain in every backup taken before the erase, and a
  restore brings them back. If you must honour an erasure completely, delete the
  affected backups too, or restore and erase again. See
  [Privacy and GDPR](privacy-gdpr.md).
- Events still in the memory buffer are not in any backup (they are not durable
  yet); `hot_rows` reports what the hot table held.
- It needs an admin session or admin API key; `deploy/backup.sh` below needs a
  stop and works without the API.

### Restoring an online backup

```bash
docker compose -f deploy/docker-compose.production.yml stop mallard
cd /srv/mallard/data
sudo mv mallard.duckdb mallard.duckdb.old; sudo rm -f mallard.duckdb.wal
sudo mv events events.old
sudo cp -a backups/before-upgrade/mallard.duckdb backups/before-upgrade/events .
sudo cp -a backups/before-upgrade/.secret backups/before-upgrade/admin.json \
  backups/before-upgrade/api_keys.json . 2>/dev/null || true
sudo chown -R 65532:65532 /srv/mallard/data
docker compose -f deploy/docker-compose.production.yml start mallard
```

Keep `mallard.duckdb.old` and `events.old` until the checks under
[Restore](#restore) pass. The restored database's `events_all` view is rebuilt at
startup, so it needs no repair.

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
