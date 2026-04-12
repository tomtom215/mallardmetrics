# Roadmap

This document describes the current state of Mallard Metrics and future directions. For a detailed history of what has shipped, see [CHANGELOG.md](CHANGELOG.md).

---

## Current Status

**v0.1.0** -- First tagged release. Core functionality complete and production-ready:

- Event ingestion (`POST /api/event`, `GET /api/event` pixel)
- Privacy-safe visitor ID generation (HMAC-SHA256 with daily salt rotation)
- Two-tier storage: DuckDB hot table + Parquet cold tier, unioned via `events_all` view
- Core metrics (unique visitors, pageviews, bounce rate)
- Dimension breakdowns (pages, sources, browsers, OS, devices, countries)
- Time-series aggregation (daily and hourly granularity)
- Behavioral analytics via the `behavioral` DuckDB extension: funnels, retention cohorts, session analysis, sequence matching, flow analysis
- Embedded dashboard SPA (Preact + HTM, no build step)
- Dashboard authentication (Argon2id), session management, API keys with scoped access
- GeoIP lookups (MaxMind GeoLite2), bot filtering, rate limiting
- Data retention cleanup, CSV/JSON export, GDPR Art. 17 erasure endpoint
- Prometheus metrics, structured logging, readiness probes, graceful shutdown
- OWASP security headers, CSRF protection, brute-force protection

**Test suite:** 333 tests (262 unit + 71 integration). Zero clippy warnings. Zero format violations.

---

## Future Considerations

These are potential directions, not committed work. They depend on real-world production usage data and should only be pursued when actual need is demonstrated.

### Performance & Scale

- **Parquet compaction** -- Merge many small Parquet files per partition into fewer large ones to improve scan performance on long retention horizons.
- **Connection pooling** -- If concurrent query load exceeds what a single DuckDB connection can handle.
- **Multi-node deployment** -- Only if a single process cannot handle the load. DuckDB is extremely fast for analytical workloads and most deployments will never need this.

### Features

- **Custom dashboard themes** -- User-configurable dashboard appearance.
- **Email reports** -- Scheduled analytics summaries delivered via SMTP.
- **Webhook notifications** -- Real-time alerts for traffic anomalies or custom thresholds.
- **Alternative auth backends** -- OIDC / SAML for enterprise dashboard access.

### Ecosystem

- **Additional GeoIP providers** -- Support IP2Location or other MMDB-compatible databases.
- **Additional export formats** -- Parquet and Arrow IPC for direct data warehouse integration.

---

## Verification Protocol

Every release must pass the full validation suite before tagging:

```bash
cargo test               # all tests pass
cargo clippy --all-targets  # zero warnings
cargo fmt -- --check     # zero format violations
cargo doc --no-deps      # docs build cleanly
```

The release workflow (`.github/workflows/release.yml`) enforces these gates automatically on every tag push.
