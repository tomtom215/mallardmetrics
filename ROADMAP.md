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
- **Operations**: embedded dashboard, Argon2id auth with persisted credential, API keys (`ReadOnly`/`Admin`), GDPR mode and erasure endpoint, Prometheus `/metrics`, health probes, alert rules and a Grafana dashboard (`deploy/monitoring/`), a backup script, a systemd unit, a hardened production compose stack, glibc release binaries and image.

---

## Known limitations

Each is a property of the design today, not a bug.

- **Behavioral analytics need a runtime download.** Funnels, retention, sessions, sequences, flow and entry/exit pages come from a DuckDB community extension (written by this project's author) fetched on first start. Offline, those endpoints answer `503`; `mallard-metrics --install-extension` seeds it ahead of time. Startup refuses a cached build older than 0.10.0.
- **Cross-day visitor analysis depends on salt rotation.** With the default daily rotation, "unique visitors" over a range counts visitor-days and weekly retention cannot work. Raising `visitor_salt_rotation_days` trades privacy for that capability; `/api/stats/retention` says which side the configuration is on.
- **Single node.** No clustering, replication or read replicas.
- **No published capacity figures.** [PERF.md](PERF.md) has micro-benchmarks only. The Parquet read path re-expands a glob across all sites and days on every query; a scoped read exists but is not yet used by the query layer (see below).
- **One admin credential.** No users, roles, per-site access or SSO; API keys have two scopes and see every site.
- **Revenue is never converted between currencies**; totals are per currency.
- **Erasure is site plus date range**; a named individual's rows cannot be singled out because visitor IDs are hashes.
- **Raw exports are built in memory**, capped at 1,000,000 rows (default 100,000). A chunked writer (`stream_raw_export` in `src/query/export.rs`) exists but the endpoint does not use it yet.
- **Bot filtering is a User-Agent substring list.** No ASN/data-centre or referrer-spam filtering.
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
| 3 | Data import (Plausible/Umami CSV first, GA4 later); the main migration blocker | Plausible (GA4 + CSV), Rybbit (Umami, Simple Analytics) | M to L |
| 4 | Scheduled email or webhook reports; spike alerts | Plausible, Swetrix, HitKeep, OpenPanel | M |
| 5 | Read-only share links per site | Plausible, Umami, HitKeep | M |
| 6 | Saved segments; comparison for breakdowns and the time series | Rybbit (saved segments), Umami | S to M |
| 7 | Stronger bot and spam defence (ASN data, referrer-spam list, flood detection) | Rybbit, Plausible Cloud | M |
| 8 | Site-management UI (add/remove sites, copy snippet) | Rybbit, Umami | M |
| 9 | Documented first-party proxy recipe for ad-blocker resilience (`/js/script.js` and `data-api` already exist; the recipe is missing) | Plausible, Umami | S |
| 10 | Search Console integration | Plausible, OpenPanel, HitKeep | M to L |
| 11 | Scroll-depth, form, 404 and engagement-time events | Plausible | S each |
| 12 | Backup/restore tooling beyond `deploy/backup.sh`; streamed and Parquet exports | HitKeep (S3 backups), GoatCounter (async CSV) | M |
| 13 | A published scale envelope (events per second, query latency at 10M and 100M rows, memory) and pre-aggregated rollups | Matomo publishes sizing tiers | M to L |
| 14 | Server-side and mobile SDKs, a client library | Rybbit, OpenPanel, Pirsch | S to M |
| 15 | MCP/AI-assistant endpoint over the existing API | Umami, Rybbit, OpenPanel, HitKeep | S to M |

Already done since the report's matrix was drawn, and therefore **not** gaps:
segment filters on every report, period comparison on the headline figures,
Prometheus metrics with alert rules, a supported backup script, glibc images
with the extension verified at build, per-IP login lockout, a persisted admin
credential.

### Engineering work that follows from this repository

- Adopt `files_for_range` / `scoped_events_sql` in the query layer. The commit that added them measured 3.0 ms against about 0.6 s for the same site-and-week question over 9,000 files (informal, see [PERF.md](PERF.md)).
- Wire the chunked raw export into `GET /api/stats/export`.
- Replace hand-rolled pieces with maintained data or crates only after benchmarking: a bot-UA list (data, not a crate), the Matomo referrer lists, the `url` crate for URL parsing, `moka` for the query cache. The ecosystem report flags each as a candidate, not a recommendation.
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
