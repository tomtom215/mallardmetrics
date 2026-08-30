# Contributing to Mallard Metrics

Thank you for your interest in contributing to Mallard Metrics. This document covers everything you need to get started.

---

## Table of Contents

- [Development Setup](#development-setup)
- [Project Structure](#project-structure)
- [Development Workflow](#development-workflow)
- [Code Standards](#code-standards)
- [Testing](#testing)
- [Benchmark Protocol](#benchmark-protocol)
- [Security Guidelines](#security-guidelines)
- [Pull Request Checklist](#pull-request-checklist)
- [Getting Help](#getting-help)

---

## Development Setup

### Prerequisites

- **Rust 1.94.0+** -- The `rust-toolchain.toml` file will install the correct version automatically via `rustup`
- **Git**
- **Disk space** -- DuckDB's bundled compilation produces large build artifacts (~27GB in debug mode). Run `cargo clean` between major rebuilds if space is constrained.

### Clone and Verify

```bash
git clone https://github.com/tomtom215/mallardmetrics.git
cd mallardmetrics

# Verify your setup compiles and all tests pass
cargo test --all-targets

# Verify zero lint warnings and formatting violations
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt -- --check
```

If all three commands succeed, your development environment is ready.

---

## Project Structure

```
mallardmetrics/
├── src/
│   ├── main.rs                  -- Entry point, background tasks, signal handling
│   ├── lib.rs                   -- Module declarations
│   ├── config.rs                -- TOML + env var configuration, validation, advisories
│   ├── server.rs                -- Axum router, middleware, routes, /metrics, health
│   ├── test_support.rs          -- Shared AppState builder for tests
│   ├── api/
│   │   ├── auth.rs              -- Authentication, sessions, API keys
│   │   ├── errors.rs            -- Error types and HTTP responses
│   │   └── stats.rs             -- All analytics endpoint handlers
│   ├── ingest/
│   │   ├── handler.rs           -- POST /api/event and the GET pixel
│   │   ├── buffer.rs            -- Bounded in-memory event buffer with flush
│   │   ├── visitor_id.rs        -- HMAC-SHA256 visitor ID and salt rotation
│   │   ├── useragent.rs         -- User-Agent and Client Hints parsing
│   │   ├── geoip.rs             -- MaxMind GeoIP reader
│   │   └── ratelimit.rs         -- Per-site and per-IP token-bucket limiters
│   ├── query/
│   │   ├── mod.rs               -- QueryScope: site, range and session window
│   │   ├── metrics.rs           -- Core metrics, including session metrics
│   │   ├── breakdowns.rs        -- Dimension breakdown queries
│   │   ├── timeseries.rs        -- Gap-filled time-bucketed aggregations
│   │   ├── realtime.rs          -- Last-N-minutes activity snapshot
│   │   ├── funnel.rs            -- Cumulative funnels (behavioral ext)
│   │   ├── retention.rs         -- Per-visitor retention cohorts (behavioral ext)
│   │   ├── sequences.rs         -- Sequence matching (behavioral ext)
│   │   ├── flow.rs              -- Flow analysis (behavioral ext)
│   │   ├── events.rs            -- Goals and custom property keys/values
│   │   ├── revenue.rs           -- Per-currency revenue totals
│   │   ├── export.rs            -- Daily and raw export as CSV or JSON
│   │   ├── cache.rs             -- TTL + LRU query cache
│   │   └── test_support.rs      -- TestDb for query tests
│   ├── storage/
│   │   ├── mod.rs               -- ReaderPool: round-robin read connections
│   │   ├── schema.rs            -- DuckDB schema, events_all view, behavioral ext
│   │   ├── parquet.rs           -- Atomic writes, compaction, retention, erasure
│   │   └── migrations.rs        -- Schema versioning
│   └── dashboard/
│       └── mod.rs               -- Embedded Preact+HTM SPA
├── tests/
│   └── ingest_test.rs           -- Integration tests (62 tests)
├── benches/
│   └── ingest_bench.rs          -- Criterion.rs benchmarks
├── src/dashboard/assets/        -- Frontend SPA files (embedded via rust-embed)
├── tracking/script.js           -- Tracking script (~3.8 KB gzipped)
├── mallard-metrics.toml.example -- Configuration template
├── Dockerfile                   -- Alpine musl builder, FROM scratch runtime
├── docker-compose.yml           -- Production-ready compose file
├── scripts/smoke-test.sh        -- End-to-end checks against the real binary
└── .github/workflows/ci.yml     -- CI pipeline (8 jobs; the first is a 4-way matrix)
```

---

## Development Workflow

### Before Starting

1. Read `DEVELOPMENT.md` for project context and module map
2. Read `LESSONS.md` for pitfalls and proven patterns
3. Establish a baseline by running the full validation suite:

```bash
cargo test --all-targets \
  && cargo clippy --all-targets --all-features -- -D warnings \
  && cargo fmt -- --check \
  && cargo doc --no-deps
```

### Making Changes

1. Create a feature branch from `main`
2. Make your changes
3. Write or update tests for every change
4. Run the full validation suite:

```bash
cargo test --all-targets \
  && cargo clippy --all-targets --all-features -- -D warnings \
  && cargo fmt -- --check \
  && cargo doc --no-deps && cargo doc --no-deps
```

5. Commit with a clear, descriptive message
6. Open a pull request

### Validation Suite

All four commands must pass before submitting a PR:

| Command | Requirement |
|---|---|
| `cargo test --all-targets` | All 551 tests pass (489 unit + 62 integration) |
| `cargo clippy --all-targets --all-features -- -D warnings` | Zero warnings (pedantic + nursery + cargo lints) |
| `cargo fmt -- --check` | Zero formatting violations |
| `cargo doc --no-deps` | Documentation builds without errors |

Tests that need the `behavioral` extension skip when it cannot be downloaded, so
a green run on an air-gapped machine does not prove they ran. CI sets
`MALLARD_REQUIRE_BEHAVIORAL=1`, which turns a skip into a failure; run it the
same way before touching any behavioral query:

```bash
MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets
```

The test suite drives the router in-process, which cannot catch a route that
only fails once the binary is assembled and listening. Run the end-to-end
checks too before changing a handler, a route, or the auth middleware:

```bash
cargo build && scripts/smoke-test.sh
```

---

## Code Standards

### Clippy

Pedantic, nursery, and cargo lint groups are all enabled. Zero warnings are tolerated. If clippy flags something that seems wrong, investigate before suppressing -- it is almost always correct.

### Formatting

Run `cargo fmt` before every commit. The CI pipeline will reject improperly formatted code.

### SQL Safety

- **Always** use parameterized queries (`$1`, `?`) for user-provided values
- **Never** interpolate user input into SQL strings via `format!()` or string concatenation
- The only exceptions are:
  - Column names from fixed enums (not user input)
  - Internal values from previous query results
- See `LESSONS.md` for background on SQL safety

### Error Handling

- Return meaningful error types (see `api/errors.rs`)
- Query functions that depend on the `behavioral` extension must degrade gracefully when it is unavailable -- return defaults or empty results, not errors
- Log errors with structured fields using `tracing`

### Privacy

- **Never** store IP addresses in DuckDB or Parquet files
- IP addresses may only be used for HMAC hashing (visitor ID) and GeoIP lookup, then must be discarded
- See `SECURITY.md` for the full privacy model

---

## Testing

### Running Tests

```bash
# All tests
cargo test

# Unit tests only
cargo test --lib

# Integration tests only
cargo test --test ingest_test

# A specific test
cargo test test_name

# With output
cargo test -- --nocapture

# Benchmarks (compilation check only)
cargo bench --no-run
```

### Writing Tests

- Every public function needs tests covering:
  - Happy path (normal usage)
  - Edge cases (empty input, boundary values)
  - Error cases (invalid input, missing dependencies)
- Integration tests go in `tests/ingest_test.rs` and test the full HTTP path: JSON request -> handler -> buffer -> DuckDB -> HTTP response
- Use `tower::ServiceExt::oneshot()` for testing Axum routers without starting a real server
- Test against real DuckDB output, not hand-written expectations (see `LESSONS.md`)
- Use `tempfile::TempDir` for test isolation -- never write to shared directories

### Test Counts

When adding tests, update the test count in `DEVELOPMENT.md` by running:

```bash
# Count unit tests
cargo test --lib 2>&1 | grep "test result"

# Count integration tests
cargo test --test ingest_test 2>&1 | grep "test result"
```

---

## Benchmark Protocol

Benchmarks use Criterion.rs 0.8 and live in `benches/ingest_bench.rs`.

### Rules

- **100 samples** per benchmark minimum
- **Run 3+ times** before comparing results
- **Report mean with 95% confidence intervals** -- never a single number
- **Improvements accepted** only when confidence intervals do not overlap
- **Document negative results** with the same rigor as positive results
- **One optimization per commit** -- never batch multiple changes into one measurement
- **No performance claims without measurement** -- "not yet measured" is the default

### Running Benchmarks

```bash
# Full benchmark suite
cargo bench

# Compilation check only (faster, for CI)
cargo bench --no-run
```

See `PERF.md` for the full benchmark framework and current baselines.

---

## Security Guidelines

Security-sensitive changes require extra care:

- **Authentication** (`api/auth.rs`) -- Timing-safe comparisons, secure cookie attributes (HttpOnly, Secure, SameSite), session expiration
- **Password hashing** -- Argon2id only, never store plaintext
- **API keys** -- SHA-256 hashed at rest, `mm_` prefix for identification
- **Input validation** -- Validate at the boundary (handler level) before data reaches the buffer or database
- **SQL injection** -- See [SQL Safety](#sql-safety) above
- **Dependencies** -- All third-party GitHub Actions must be pinned to commit SHAs

If you discover a security vulnerability, see `SECURITY.md` for responsible disclosure.

---

## Pull Request Checklist

Before submitting your PR, verify every item:

- [ ] `cargo test --all-targets` -- all tests pass
- [ ] `MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets` -- behavioral tests really ran
- [ ] `scripts/smoke-test.sh` -- the real binary still serves every route
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` -- zero warnings
- [ ] `cargo fmt -- --check` -- zero formatting violations
- [ ] `cargo doc --no-deps` -- documentation builds without errors
- [ ] New or changed functionality has corresponding tests
- [ ] No SQL injection vectors introduced (parameterized queries used)
- [ ] No PII stored (IP addresses only for hashing/GeoIP, then discarded)
- [ ] CHANGELOG.md updated with your changes
- [ ] Documentation updated if applicable (README.md, DEVELOPMENT.md)

---

## Getting Help

- Open an issue on the [GitHub repository](https://github.com/tomtom215/mallardmetrics) for bugs or feature requests
- Read `LESSONS.md` for common pitfalls and their solutions
- Read `PERF.md` for benchmark methodology
- Read `ROADMAP.md` for planned features and current status
