# Development Guide

## Project Overview

Mallard Metrics is a self-hosted, cookieless web-analytics server: one Rust
process with an embedded DuckDB database and dashboard. What it is and is not is
stated in [README.md](README.md#what-it-is-and-what-it-is-not); this file is for
people changing the code.

## Architecture

- **Language**: Rust, edition 2024 (MSRV 1.98.0; `Cargo.toml` `rust-version` and `rust-toolchain.toml` must agree, CI checks it)
- **Web framework**: Axum 0.8
- **Database**: DuckDB 1.5.6, embedded, via the `duckdb` crate 1.10506.1 (`bundled`, `parquet`, `chrono` features)
- **Analytics**: the `behavioral` community extension, 0.10.0 or newer, installed and loaded at runtime. Startup refuses an older cached build (`MIN_BEHAVIORAL_VERSION` in `src/storage/schema.rs`); see LESSONS L22
- **Storage**: a DuckDB hot table plus date-partitioned Parquet, unioned by the `events_all` view
- **Frontend**: Preact + HTM, no build step, embedded in the binary
- **Deployment**: glibc binary on `gcr.io/distroless/cc-debian12:nonroot` (UID 65532). There is no musl build because the extension is not published for musl (LESSONS L29)

The module map lives in one place:
[`docs/src/architecture.md`](docs/src/architecture.md#module-map).

## Build and Test Commands

`make help` lists shortcuts (`make check` runs the four Rust gates). The
commands behind them:

```bash
# Build
cargo build

# Run all tests (759 tests at the time of writing; see Current Metrics)
cargo test --all-targets

# Behavioral-extension tests skip when the extension cannot be downloaded.
# Setting this turns a skip into a failure, which is what CI does.
MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets

# End-to-end against the real binary on a real socket. The in-process tests
# cannot see a route that fails only once the server is assembled.
cargo build && scripts/smoke-test.sh

# Nothing in the Rust suite runs the dashboard's JavaScript.
node scripts/check-dashboard-methods.mjs src/dashboard/assets/app.js
node scripts/check-dashboard-browser.mjs http://127.0.0.1:8000   # needs Playwright

# Clippy (zero warnings required)
cargo clippy --all-targets --all-features -- -D warnings

# Format check
cargo fmt -- --check

# Documentation. CI sets RUSTDOCFLAGS globally; a local shell does not, and
# `cargo doc` exits 0 on a broken intra-doc link without it.
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# Run the server
cargo run

# Run benchmarks
cargo bench
```

## Quality Standards

- Zero clippy warnings (pedantic, nursery and cargo lint groups enabled)
- Zero formatting violations
- All tests pass. The one `#[ignore]`d test is a timing harness, run by hand
- Documentation builds without errors
- Every claim in this file must be verifiable by running the relevant command

## Current Metrics

Measured on 2026-10-21 at the commit that introduced this table, with
`MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets`.

| Metric | Value | Verified by |
|---|---|---|
| Unit tests (lib) | 683 passed, 1 ignored | `cargo test --lib` (the ignored one is a timing harness in `src/storage/parquet.rs`) |
| Integration tests | 76 | `cargo test --test ingest_test` |
| Total passing | 759 | `MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets` |
| Doc tests | 0 | `cargo test --doc` |
| Property-test suites | 3 | `src/query/cache.rs`, `src/ingest/ratelimit.rs`, `src/ingest/visitor_id.rs` |
| Clippy warnings | 0 | `cargo clippy --all-targets --all-features -- -D warnings` |
| Format violations | 0 | `cargo fmt -- --check` |
| CI job definitions | 18 | `ci.yml` 9, `pages.yml` 2, `release.yml` 6, `security.yml` 1 |
| Tracking script, gzipped | 4,013 bytes | `gzip -9 -c tracking/script.js \| wc -c` (budget 4,096) |

CI runs 18 job definitions across four workflows. A push to `main`
expands the `ci.yml` `ci` matrix (Test, Clippy, Format, Documentation) into four
job instances.

## Development Workflow

1. Run the full validation suite before and after changes:
   ```bash
   cargo test --all-targets \
     && cargo clippy --all-targets --all-features -- -D warnings \
     && cargo fmt -- --check \
     && RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
   ```
2. Verify claims with actual command output.
3. After every test run, grep the documentation for the previous test count and
   replace it (LESSONS L16).
4. `tracking/script.js` is served on every page view. The `static-checks` CI job
   holds it to a 4,096-byte gzipped budget (`gzip -9`; currently 4,013
   bytes). Growing past it should be a decision, and the sizes quoted in the
   README and docs updated with it.
5. Several worktrees sharing one `CARGO_TARGET_DIR` can run each other's test
   binaries, because cargo's package hash ignores the worktree path. `touch`
   your sources before building and run a copied binary (LESSONS L36).

## Development Guidelines

- Do not claim performance numbers without Criterion measurement with confidence intervals
- Do not claim test counts without running `cargo test` and counting from output
- Do not guess SQL semantics; test with actual DuckDB
- Do not interpolate user input into SQL (use parameters)
- Do not store IP addresses; they are used only for hashing and GeoIP
- Do not use SQL clock functions (`NOW()`, `CURRENT_TIMESTAMP`) against event
  data. They return `TIMESTAMP WITH TIME ZONE`, whose interval arithmetic lives
  in DuckDB's ICU extension and whose cast to a naive `TIMESTAMP` follows the
  host's time zone, while every stored timestamp is naive UTC. Compute the
  instant in Rust with `Utc::now().naive_utc()` and bind it as a parameter.
