# Quick Start

This gets Mallard Metrics running and collecting events. The commands in
"Try it from source" and "Verify events" were run against a built binary exactly
as written; the Docker commands were checked against the repository files but not
run (the machine that wrote this page has no Docker daemon).

## Prerequisites

- Docker (recommended), **or** a Linux or macOS host with a C/C++ toolchain and
  Rust 1.98.0 to build from source (the repository pins it in
  `rust-toolchain.toml`). Windows builds are not supported by the release
  pipeline.
- A site to track.
- Outbound network access on first start, so DuckDB can download the `behavioral`
  extension (a few MB). Without it Mallard still runs, but funnels, retention,
  sessions, sequences and flow answer `503`
  ([Troubleshooting](troubleshooting.md#behavioral-endpoints-answer-503)).

## Option 1: Docker

```bash
docker run -d --name mallard-metrics --stop-timeout 45 \
  -p 127.0.0.1:8000:8000 \
  -v mallard-data:/data \
  -e MALLARD_SECRET="$(openssl rand -hex 32)" \
  -e MALLARD_ADMIN_PASSWORD='choose-a-long-password' \
  ghcr.io/tomtom215/mallardmetrics:0.2.0
```

- `MALLARD_SECRET` must be **at least 16 characters**; the server refuses to start
  otherwise. Keep it: a new secret re-identifies every visitor.
- `MALLARD_ADMIN_PASSWORD` must be **at least 12 characters**. If you omit it, the
  dashboard and all reports are **open to anyone who can reach the port** until you
  set a password in the dashboard.
- `127.0.0.1:` keeps the port private to the host. For anything public, put a TLS
  reverse proxy in front: [Deployment](deployment.md).
- The image is glibc-based and runs as UID 65532. Pin a version tag.

Open `http://localhost:8000`.

## Option 2: Docker Compose

```bash
git clone https://github.com/tomtom215/mallardmetrics.git && cd mallardmetrics
echo "MALLARD_SECRET=$(openssl rand -hex 32)" > .env
echo "MALLARD_ADMIN_PASSWORD=choose-a-long-password" >> .env
docker compose up -d --build
```

The first build compiles DuckDB and takes several minutes. The compose file
binds `127.0.0.1:8000`, runs read-only with no capabilities, and waits 45 seconds
on stop so the buffer can flush.

## Option 3: Try it from source

```bash
git clone https://github.com/tomtom215/mallardmetrics.git && cd mallardmetrics
cargo build --release          # compiles DuckDB: several minutes, several GB of disk
mkdir -p /tmp/mallard-try
MALLARD_DATA_DIR=/tmp/mallard-try \
MALLARD_HOST=127.0.0.1 \
MALLARD_FLUSH_INTERVAL=5 \
  ./target/release/mallard-metrics
```

You will see two warnings (a one-day salt rotation, and no `site_ids`/per-IP limit);
they are advisories, not errors. The line `Behavioral extension loaded
version="0.10.0"` means everything is available; `Listening addr=127.0.0.1:8000`
means it is ready. (`MALLARD_FLUSH_INTERVAL=5` only makes the demo quicker; the
default is 60 seconds.)

With a config file instead: `./target/release/mallard-metrics mallard-metrics.toml.example`
(every value in the example is the default; its `data_dir` is the relative path `data`).

## Set the admin password

With no password configured the dashboard is open and shows a banner. Either set
`MALLARD_ADMIN_PASSWORD`, or click **Set admin password** in the dashboard (the
password must be 12+ characters), or:

```bash
curl -X POST http://localhost:8000/api/auth/setup \
  -H 'Content-Type: application/json' \
  -d '{"password": "choose-a-long-password"}'
# {"status":"ok"}
```

Do this before exposing the instance: until a password exists, whoever reaches
`/api/auth/setup` first claims it.

## Embed the tracking script

```html
<script defer
        src="https://your-mallard-instance.com/mallard.js"
        data-domain="your-site.com"></script>
```

- Replace the origin with your instance's URL and `your-site.com` with the site ID
  you want to report under (it must be in `site_ids` if you set that).
- The script is about 4 KB gzipped, sets no cookies, fires a pageview on load and
  on every in-page route change, and sends the URL, UTM parameters, viewport width
  and the referrer (only when it comes from another origin).
- **It does not send from `localhost`, `file:` URLs or private addresses** unless
  you add `data-include-local="true"`, so testing from your laptop shows nothing.

All options and custom events: [Tracking Script](tracking-script.md).

## Verify events

Send one event by hand (with a browser-like User-Agent: curl's default is classed
as a bot and silently dropped):

```bash
curl -i -X POST http://localhost:8000/api/event \
  -H 'Content-Type: application/json' \
  -H 'User-Agent: Mozilla/5.0 (X11; Linux x86_64) Chrome/131.0 Safari/537.36' \
  -d '{"d":"my-site.test","n":"pageview","u":"https://my-site.test/hello"}'
# HTTP/1.1 202 Accepted
```

Events sit in memory until the next flush (every `flush_interval_secs`, default 60),
**so reports lag by up to a minute**:

```bash
curl -s http://localhost:8000/health/detailed | grep buffered_events   # "buffered_events":1
sleep 6        # with MALLARD_FLUSH_INTERVAL=5
curl -s "http://localhost:8000/api/stats/main?site_id=my-site.test&period=day"
# {"unique_visitors":1,"total_pageviews":1,"total_events":1,...,"behavioral_available":true}
```

(Add `-H "X-API-Key: mm_..."` or log in first if you set a password.) Then pick
`my-site.test` in the dashboard.

## The dashboard

At `http://localhost:8000`: choose the site, a period (Today, 7D, 30D, 90D, 12M or
custom dates) and optionally a comparison with the previous period or last year;
filter any panel by clicking a row.

- **Overview** — visitors, pageviews, events, views per visitor, visits, bounce
  rate, visit duration, views per visit, and a visitors/pageviews chart (with a
  table view). Export buttons give daily CSV, raw CSV and raw JSON.
- **Right now** — visitors in the last five minutes.
- **Pages, Acquisition, Locations, Devices** — breakdowns, including entry and exit
  pages, UTM parameters, regions, cities and versions.
- **Goals & custom events**, **Revenue**.
- **Funnel**, **Retention cohorts**, **Sequence**, **Flow**.
- **API keys** and **Install snippet** buttons; the state of the view is in the URL,
  so a view can be bookmarked or shared.

![Mallard Metrics Dashboard](dashboard-screenshot.png)

## What's next

- [Configuration](configuration.md) — every setting.
- [Deployment](deployment.md) — TLS, proxies, production hardening.
- [Metric Definitions](metric-definitions.md) — what each number means.
- [Backup and Restore](backup-restore.md) and [Troubleshooting](troubleshooting.md).
