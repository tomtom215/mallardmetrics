# Quick Start

This guide gets Mallard Metrics running and collecting events in a few minutes.

## Prerequisites

- Docker (recommended), or a Linux/macOS host with Rust 1.93+ for building from source.
- A web property you want to track.

---

## Option 1: Docker (Recommended)

```bash
docker run -p 8000:8000 \
  -v mallard-data:/data \
  -e MALLARD_SECRET=your-random-32-char-secret \
  -e MALLARD_ADMIN_PASSWORD=your-dashboard-password \
  ghcr.io/tomtom215/mallard-metrics
```

Open `http://localhost:8000` to access the dashboard.

## Option 2: Docker Compose

Download `docker-compose.yml` from the repository root and run:

```bash
docker compose up -d
```

The compose file includes persistent storage, restart policy, and environment variable configuration. Set `MALLARD_SECRET` and `MALLARD_ADMIN_PASSWORD` in your shell or a `.env` file before running.

## Option 3: Build from Source

```bash
git clone https://github.com/tomtom215/mallardmetrics
cd mallardmetrics
cargo build --release
./target/release/mallard-metrics mallard-metrics.toml.example
```

> **Note:** The `bundled` feature for DuckDB means no external libduckdb is required. The build will take a few minutes the first time as DuckDB is compiled from source.

---

## Step 2: Embed the Tracking Script

Add the tracking script to every page you want to track. Place it in the `<head>` or at the end of `<body>`:

```html
<script
  async
  defer
  src="https://your-mallard-instance.com/mallard.js"
  data-domain="your-site.com">
</script>
```

Replace:
- `https://your-mallard-instance.com` with the URL of your Mallard Metrics instance.
- `your-site.com` with the domain you configured in `site_ids` (or any domain if `site_ids` is empty).

The script is under 1 KB, loads asynchronously, sets no cookies, and automatically tracks `pageview` events including URL, referrer, UTM parameters, screen size, and User-Agent.

See [Tracking Script](tracking-script.md) for the full API including custom events and revenue tracking.

---

## Step 3: Verify Events Are Arriving

Check the health endpoint:

```bash
curl http://localhost:8000/health
# ok

curl http://localhost:8000/health/detailed
# {"status":"ok","version":"0.1.0","buffered_events":3,...}
```

Events are held in a memory buffer before being flushed to disk. You can query the dashboard immediately — the `events_all` view unions the hot buffer and all persisted Parquet data automatically.

---

## Step 4: Dashboard

Navigate to `http://localhost:8000` in your browser.

If you set `MALLARD_ADMIN_PASSWORD`, you will be prompted to log in. The dashboard shows:

- **Overview** — Unique visitors, pageviews, bounce rate, session metrics.
- **Timeseries** — Visitors and pageviews charted over your selected period.
- **Breakdowns** — Top pages, referrer sources, browsers, OS, devices, countries.
- **Funnel** — Define a conversion funnel with up to N steps.
- **Retention** — Weekly cohort retention grid.
- **Sequences** — Behavioral pattern matching and conversion rates.
- **Flow** — Next-page navigation from any starting page.

---

## What's Next?

- [Configuration](configuration.md) — All configuration options.
- [Tracking Script](tracking-script.md) — Custom events and revenue tracking.
- [API Reference](api-reference/index.md) — Integrate programmatically.
- [Deployment](deployment.md) — Production deployment guides.
