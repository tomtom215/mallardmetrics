# Roadmap

Where Mallard Metrics stands, what it cannot do, and what is missing for wider
use. For what has shipped, see [CHANGELOG.md](CHANGELOG.md). Nothing below is
scheduled; items are ordered by evidence of need, not by commitment.

---

## Status

Implemented and covered by the test suite (counts in
[DEVELOPMENT.md](DEVELOPMENT.md#current-metrics)):

- **Ingestion**: `POST /api/event`, a pixel `GET`, per-site and per-client-IP rate limits, bounded buffering, bot filtering, GeoIP, User-Agent and Client Hints parsing.
- **Storage**: DuckDB hot table plus date-partitioned Parquet, unioned by `events_all`; crash-safe flush and compaction; retention; consistent reads.
- **Analytics**: core and session metrics, twenty breakdown dimensions, segment filters on every report, period comparison on `/api/stats/main`, goals, custom properties, per-currency revenue, realtime, and (with the `behavioral` extension) funnels, retention, sequences and flow; CSV/JSON export.
- **Sharing and segments**: revocable read-only share links per site (optionally limited to a saved segment, which cuts events rather than selecting visits), and named saved segments.
- **Data in and out**: an offline importer for the GA4 BigQuery export, streamed raw export, a Parquet export for warehouses, and an online backup endpoint.
- **Tracker**: an optional extras script (scroll depth, engagement time, 404, form submit) with its own size budget.
- **Notifications**: signed webhook alerts (flush stalled, extension missing, cold tier degraded, traffic drop or spike against the same hour of previous weeks) and scheduled daily or weekly reports, with address checks that refuse internal targets.
- **Operations**: embedded dashboard, Argon2id auth with persisted credential, API keys (`ReadOnly`/`Admin`), GDPR mode and erasure endpoint, Prometheus `/metrics`, health probes, alert rules and a Grafana dashboard (`deploy/monitoring/`), a backup script, a systemd unit, a hardened production compose stack, glibc release binaries and image.

---

## Known limitations

Each is a property of the design today, not a bug.

- **Behavioral analytics need a runtime download.** Funnels, retention, sessions, sequences, flow and entry/exit pages come from a DuckDB community extension (written by this project's author) fetched on first start. Offline, those endpoints answer `503`; `mallard-metrics --install-extension` seeds it ahead of time. Startup refuses a cached build older than 0.10.0.
- **Cross-day visitor analysis depends on salt rotation.** With the default daily rotation, "unique visitors" over a range counts visitor-days and weekly retention cannot work. Raising `visitor_salt_rotation_days` trades privacy for that capability; `/api/stats/retention` says which side the configuration is on.
- **Single node.** No clustering, replication or read replicas.
- **No published capacity figures.** [PERF.md](PERF.md) has micro-benchmarks only; the scoped read path is measured on 100 sites by 90 days, not at 10M or 100M rows.
- **One admin credential.** No users, roles, per-site access or SSO; API keys have two scopes and see every site.
- **Revenue is never converted between currencies**; totals are per currency.
- **Erasure is site plus date range**; a named individual's rows cannot be singled out because visitor IDs are hashes.
- **Raw exports are capped** at 1,000,000 rows (default 100,000) and stream, so a failure after the first byte aborts the connection instead of returning a status.
- **Bot filtering is a User-Agent substring list**, plus an opt-in datacenter-ASN filter that needs a database you supply. No referrer-spam or flood detection.
- **Imports cover one product.** Only the GA4 BigQuery event export is importable. Plausible and Fathom export daily aggregates, and Umami and Matomo publish no field-level export schema we could verify; see [Importing data](docs/src/importing-data.md).
- **Webhook notifications are one URL**, with state in memory: a restart forgets which alerts were firing. There is no email delivery.
- **Share links expose drill-down.** A viewer can combine filters until a count reaches one visitor; a link is not an aggregates-only view (see the [share links](docs/src/share-links.md) threat model).
- **Session analyses are unavailable under hour-rounded timestamps** (`gdpr_mode`).
- **`site_ids` comes from configuration**, not a UI; there is no site-management screen.

---

## Gaps for production use

Ranked from an ecosystem comparison done on 2026-10-20 that read competitors'
repositories and documentation pages. Its competitor figures were partly
unverified, so none are repeated here; "who ships it" cites only what the report
read on a named page. Effort: S under a week, M one to three weeks, L longer
(estimates for the current architecture).

| # | Gap | Evidence others ship it | Effort |
|---|---|---|---|
| 1 | Multiple users, roles and per-site access | Umami (Teams), Rybbit (organisations), HitKeep (members) | L |
| 2 | OIDC single sign-on | Plausible has it outside Community Edition; HitKeep lists it on a paid plan | M, after 1 |
| 3 | More importers: Plausible and Fathom need an aggregates table (their exports are not events); Umami and Matomo need a documented event schema first | Plausible (GA4 + CSV), Rybbit (Umami, Simple Analytics) | M |
| 4 | Email delivery of reports; per-site webhooks; persisted alert state | Plausible, Swetrix, HitKeep, OpenPanel | S to M |
| 5 | Comparison for breakdowns and the time series | Rybbit, Umami | S to M |
| 6 | Referrer-spam list and flood detection (the ASN filter is done) | Rybbit, Plausible Cloud | M |
| 7 | Site-management UI (add/remove sites, copy snippet) | Rybbit, Umami | M |
| 8 | Documented first-party proxy recipe for ad-blocker resilience (`/js/script.js` and `data-api` already exist; the recipe is missing) | Plausible, Umami | S |
| 9 | Search Console integration | Plausible, OpenPanel, HitKeep | M to L |
| 10 | A published scale envelope (events per second, query latency at 10M and 100M rows, memory) and pre-aggregated rollups | Matomo publishes sizing tiers | M to L |
| 11 | Server-side and mobile SDKs, a client library | Rybbit, OpenPanel, Pirsch | S to M |
| 12 | MCP/AI-assistant endpoint over the existing API | Umami, Rybbit, OpenPanel, HitKeep | S to M |

Already done since the report's matrix was drawn, and therefore **not** gaps:
segment filters on every report, saved segments, read-only share links, period
comparison on the headline figures, Prometheus metrics with alert rules, webhook
alerts and scheduled reports, a GA4 importer, an online backup endpoint, streamed
and Parquet exports, scroll-depth, engagement, 404 and form events, a datacenter
ASN filter, scoped Parquet reads, a supported backup script, glibc images with
the extension verified at build, per-IP login lockout, a persisted admin
credential.

### Engineering work that follows from this repository

- Replace hand-rolled pieces with maintained data or crates only after benchmarking: a bot-UA list (data, not a crate), the Matomo referrer lists, the `url` crate for URL parsing. `moka` for the query cache was evaluated and not adopted: inserting into a full cache measured 40-44 µs at 1,000 entries and about 0.4 ms at the default 10,000, which does not justify the code ([PERF.md](PERF.md)). The ecosystem report flags each of the others as a candidate, not a recommendation.
- Evaluate DuckLake for the hot/cold tiering as a branch prototype; its maintenance functions and extension-download story were not verified.

### Positioning

Funnels, journeys and retention are absent from Plausible Community Edition
according to Plausible's documentation; Umami, Rybbit, OpenPanel and HitKeep list
funnels, so they are not a differentiator against those. What Mallard offers that
the same search did not find elsewhere is the combination of a single Rust
binary, embedded DuckDB, an open Parquet cold tier and documented salt-rotation
trade-offs. HitKeep (Go, embedded DuckDB, MIT) overlaps this architecture and
ships more product features today; Mallard is currently a capable engine with a
thin product around it.

---

## Verification protocol

Every release must pass the validation suite before tagging; the release
workflow (`.github/workflows/release.yml`) enforces it on every version tag.

```bash
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt -- --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# Behavioral tests skip when the extension cannot be downloaded; CI turns a skip
# into a failure with this variable.
MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets

# End-to-end against the real binary: every route, on a socket, with real storage.
cargo build && scripts/smoke-test.sh
```
