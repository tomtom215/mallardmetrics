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

- **Rust 1.98.0** -- `rust-toolchain.toml` pins it and `rustup` installs it automatically
- **Git**
- **Disk space** -- DuckDB is compiled from source. Each distinct feature/profile combination (`cargo build`, `test`, `clippy`, `doc`) gets its own `libduckdb-sys` build directory, about 1.1 GB each with the repository's `debug = false` setting for that crate (LESSONS L4); a shared `target/` serving several worktrees measured 11 GB on 2026-10-21. Check `df -h` before concluding a build is hung: a full disk makes the C++ compile crawl rather than fail.

### Clone and Verify

```bash
git clone https://github.com/tomtom215/mallardmetrics.git
cd mallardmetrics

# Verify your setup compiles and all tests pass
# (behavioral tests need network access on first run to fetch the extension)
cargo test --all-targets

# Verify zero lint warnings and formatting violations
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt -- --check
```

If all three commands succeed, your development environment is ready.

---

## Project Structure

The per-module map (`src/api`, `src/ingest`, `src/query`, `src/storage`) is
kept in one place: [`docs/src/architecture.md`](docs/src/architecture.md#module-map).
At the top level:

```
src/                  Rust crate (library + the mallard-metrics binary)
src/dashboard/assets/ Preact + HTM dashboard, embedded via rust-embed
tests/ingest_test.rs  Integration tests (full HTTP path, in-process)
benches/              Criterion benchmarks
tracking/script.js    The tracking script, served at /mallard.js
scripts/              smoke-test.sh, dashboard checks, seed data, Docker and ABI checks
deploy/               Production compose, Caddyfile, backup, systemd, monitoring
docs/                 mdBook documentation site
.github/workflows/    ci.yml, pages.yml, release.yml, security.yml
Dockerfile, docker-compose.yml, mallard-metrics.toml.example, Makefile
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
  && RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
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
  && RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

5. Commit with a clear, descriptive message
6. Open a pull request

### Validation Suite

All four commands must pass before submitting a PR:

| Command | Requirement |
|---|---|
| `cargo test --all-targets` | All tests pass (current count: [DEVELOPMENT.md](DEVELOPMENT.md#current-metrics)) |
| `cargo clippy --all-targets --all-features -- -D warnings` | Zero warnings (pedantic + nursery + cargo lints) |
| `cargo fmt -- --check` | Zero formatting violations |
| `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` | Documentation builds without errors |

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

Nothing in the Rust suite executes the dashboard's JavaScript. `node --check`
sees only syntax, so `scripts/check-dashboard-methods.mjs` (which CI runs)
catches a handler wired to a method that no longer exists. For anything more —
a render crash, a fetch to a route that moved — drive it in a browser:

```bash
node scripts/check-dashboard-browser.mjs http://127.0.0.1:8000
```

That one needs Playwright, which is why it is a local script rather than a CI
job. Run it before shipping a dashboard change.

An empty dashboard exercises very little, so seed it first —
`scripts/seed-demo-data.py` writes 30 days of synthetic events with recurring
visitors, multi-step journeys, revenue and custom events, which is enough for
every panel to render something.

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
- Report a missing `behavioral` extension honestly: endpoints that need it return `503` with code `behavioral_unavailable`, and the session fields of `/api/stats/main` are `null`. Never return an empty `200` or a `0` that reads as "no data"
- Log errors with structured fields using `tracing`

### Privacy

- **Never** store IP addresses in DuckDB or Parquet files
- IP addresses may only be used for HMAC hashing (visitor ID) and GeoIP lookup, then must be discarded. The one exception is the truncated address (`anonymize_ip`) in authentication log lines
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

If several worktrees share one `CARGO_TARGET_DIR`, `touch` your sources before
building and run a copied binary: the package hash ignores the worktree path, so
cargo can otherwise run another worktree's test binary (LESSONS L36).

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

When you add tests, update the table in [DEVELOPMENT.md](DEVELOPMENT.md#current-metrics)
and grep every document for the old total (LESSONS L16). The counts come from
the `test result:` lines of:

```bash
MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets 2>&1 | grep "test result"
```

---

## Benchmark Protocol

Benchmarks use Criterion.rs 0.8 and live in `benches/ingest_bench.rs`.

### Rules

- **100 samples** per benchmark minimum (the `query_scale` benchmark uses 10 because one iteration takes about a second; its figures are informal for that reason)
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
- [ ] `scripts/docker-smoke.sh <image>` -- if the Dockerfile or `deploy/` changed
- [ ] `node scripts/check-dashboard-browser.mjs` -- if the dashboard changed
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` -- zero warnings
- [ ] `cargo fmt -- --check` -- zero formatting violations
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` -- documentation builds without errors
- [ ] New or changed functionality has corresponding tests
- [ ] No SQL injection vectors introduced (parameterized queries used)
- [ ] No PII stored (IP addresses only for hashing/GeoIP, then discarded)
- [ ] CHANGELOG.md `[Unreleased]` updated with your changes
- [ ] Documentation updated where behaviour changed (README.md, `docs/src/`, DEVELOPMENT.md test counts)

---

## Getting Help

- Open an issue on the [GitHub repository](https://github.com/tomtom215/mallardmetrics) for bugs or feature requests
- Read `LESSONS.md` for common pitfalls and their solutions
- Read `PERF.md` for benchmark methodology
- Read `ROADMAP.md` for planned features and current status
