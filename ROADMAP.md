# Roadmap

This document describes the current state of Mallard Metrics and future directions. For a detailed history of what has shipped, see [CHANGELOG.md](CHANGELOG.md).

---

## Current Status

Core functionality is complete and production-ready:

**Ingestion**
- `POST /api/event` plus a `GET` pixel endpoint for JavaScript-free contexts
- Per-site HMAC-SHA256 visitor IDs with configurable salt rotation
- User-Agent and Client Hints parsing, bot filtering, GeoIP resolution
- Per-site and per-IP rate limiting, bounded buffering, atomic Parquet writes

**Storage**
- Two-tier DuckDB hot table plus Parquet cold tier, unioned by `events_all`
- Date-partitioned layout with automatic compaction and retention cleanup
- A read-connection pool so queries do not block ingestion

**Analytics**
- Visitors, pageviews, events, visits, bounce rate, visit duration
- Twenty breakdown dimensions, covering every column the ingest path populates
- Gap-filled time series, realtime activity, goals, custom properties, revenue
- Behavioral analytics: cumulative funnels with ordering modes, per-visitor
  retention cohorts, session metrics, sequence matching, forward/backward flow
- Daily and raw event exports as CSV or JSON

**Operations**
- Embedded dashboard SPA (Preact + HTM, no build step) with dark mode
- Argon2id authentication, scoped API keys, brute-force protection
- GDPR mode, configurable privacy flags, Art. 17 erasure endpoint
- Prometheus metrics, structured logging, readiness probes, graceful shutdown
- OWASP security headers, CSRF protection, non-root container image

---

## Known limitations

Stated plainly, because each is a property of the design rather than a bug:

- **Cross-period visitor analysis depends on salt rotation.** With the default
  daily rotation, "unique visitors" over a longer range counts visitor-days, and
  weekly retention cohorts cannot work. `visitor_salt_rotation_days` trades
  privacy for that capability, and the retention endpoint reports which side of
  the trade the current configuration sits on.
- **Behavioral analytics need a community extension** downloaded at startup.
  Without network access on first run, those endpoints return 503.
- **Single-node.** There is no clustering, replication or read-replica story.
  DuckDB handles a great deal on one box, but the ceiling is one box.
- **Revenue is not converted between currencies.** There is no exchange-rate
  source, so totals are reported per currency and never summed.
- **Erasure is site plus date range.** Visitor IDs are pseudonymous hashes, so a
  named individual's rows cannot be singled out.
- **Exports are built in memory, not streamed.** `limit` therefore bounds memory
  as well as row count. Slice by date range rather than raising the ceiling on a
  small host.

---

## Future Considerations

Potential directions, not committed work. Each depends on demonstrated need.

### Performance and scale

- **Time-partitioned Parquet statistics** — record per-file min/max timestamps so
  a narrow date range can skip files without opening their footers.
- **Incremental pre-aggregation** — materialise daily rollups for ranges where
  exact per-event detail is not needed.
- **Multi-node deployment** — only if a single process demonstrably cannot cope.

### Features

- **Alerting** — thresholds and anomaly detection on traffic or conversions.
- **Scheduled reports** — periodic summaries by email or webhook.
- **Saved segments** — named filters reusable across every report.
- **UTM-aware attribution models** — first-touch and last-touch, rather than the
  per-event attribution available today.
- **Alternative auth backends** — OIDC or SAML for organisations that need it.

### Ecosystem

- **Additional GeoIP providers** — IP2Location and other MMDB-compatible databases.
- **Additional export formats** — Parquet and Arrow IPC for warehouse ingestion.
- **Streaming exports** — a chunked response body, removing the memory bound
  that `limit` currently doubles as.
- **A client library** — a thin wrapper over the ingest endpoint for server-side
  events in common languages.

---

## Verification Protocol

Every release must pass the full validation suite before tagging:

```bash
cargo test --all-targets   # all 551 tests pass
cargo clippy --all-targets --all-features -- -D warnings   # zero warnings
cargo fmt -- --check       # zero format violations
cargo doc --no-deps        # docs build cleanly

# Behavioral-extension tests skip when the extension cannot be downloaded.
# CI sets this so a skip becomes a failure.
MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets

# End-to-end against the real binary: every route, on a socket, with real
# storage. The in-process tests cannot see a route that only breaks once the
# server is assembled.
cargo build && scripts/smoke-test.sh
```

The release workflow (`.github/workflows/release.yml`) enforces these gates automatically on every tag push.
