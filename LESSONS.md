# Development Lessons

Lessons learned during Mallard Metrics development, organized by category. Each lesson includes context on when it was discovered and why it matters.

---

## Table of Contents

- [Build and Dependencies](#build-and-dependencies)
- [Testing](#testing)
- [Architecture](#architecture)
- [Security](#security)
- [Inherited Lessons from duckdb-behavioral](#inherited-lessons-from-duckdb-behavioral)

---

## Build and Dependencies

### L1: DuckDB COPY TO does not support parameterized queries

**Session 1.** DuckDB's `COPY ... TO` statement cannot use `$1`-style parameterized queries. Values must be interpolated into the SQL string. For internal values (site_id, date from the events table itself), this is safe. For user-provided values, use parameterized queries in the SELECT, not in COPY.

### L2: DuckDB Parquet extension must be bundled

**Session 1.** The DuckDB Rust crate requires the `parquet` feature flag (`duckdb = { features = ["bundled", "parquet"] }`) to include Parquet support at compile time. Without it, DuckDB tries to auto-download the extension at runtime, which fails in environments without network access (CI, containers, tests).

### L3: Rust edition 2024 features require toolchain >= 1.85

**Session 1.** Transitive dependencies (e.g., `getrandom 0.4.x`) may require edition 2024, which is only supported in Rust 1.85.0+. Pin the MSRV to at least 1.85.0.

### L4: DuckDB bundled compilation uses significant disk space

**Session 1.** The `duckdb` crate with `bundled` + `parquet` features produces ~27GB of build artifacts in debug mode. This can exhaust disk space in constrained environments. Use `cargo clean` between major rebuilds.

---

## Testing

### L5: Test against real DuckDB output, not hand-written expectations

**Inherited from duckdb-behavioral.** SQL test expectations must be validated against actual DuckDB output. Date formatting, type casting, and NULL handling can differ from expectations.

### L6: DuckDB date formatting varies by context

**Session 1.** `CAST(timestamp AS DATE)` returns a Date type whose string representation may vary. Use `STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d')` for consistent string formatting in queries that need to compare dates as strings.

### L7: iPhone User-Agent strings contain "Mac OS X"

**Session 1.** iPhone UA strings like `"iPhone; CPU iPhone OS 17_2_1 like Mac OS X"` contain the substring "Mac OS X". Detection logic must check for iPhone/iPad before macOS to avoid misclassification.

### L8: Substring matching for referrer sources has collision risks

**Session 1.** `"reddit.com".contains("t.co")` is true because `reddit.com` contains the substring `t.co` at position 5 (`reddi[t.co]m`). Use exact hostname matching (`host == "t.co"`) for short domain names to avoid false positives.

---

## Architecture

### L9: Behavioral extension availability is runtime-dependent

**Session 1.** The `behavioral` extension is installed from the DuckDB community repository at runtime. Unit tests cannot assume it is available. Queries using `sessionize`, `window_funnel`, `retention`, etc. must gracefully handle the extension being absent. Use `unwrap_or(default)` for metrics that depend on behavioral functions.

### L10: E2E testing is non-negotiable

**Inherited from duckdb-behavioral.** Unit tests alone miss integration boundary bugs. HTTP API integration tests validate the full path: JSON -> handler -> buffer -> DuckDB -> response.

### L11: Axum Tower middleware composes cleanly

**Session 1.** CORS, tracing, and compression are added as Tower layers with no impedance mismatch. The `tower::ServiceExt` trait enables testing routers with `oneshot()` without starting a real server.

---

## Security

### L12: Never interpolate user input into SQL strings

**Session 1.** All user-provided values (site_id, dates, event names) must use parameterized queries (`$1`, `?`). The only exceptions are column names from fixed enums and internal values from previous query results.

### L13: Input validation at the boundary

**Session 1.** Validate all inbound event data for type, length, and format in the handler before passing to the buffer. Sanitize strings by removing control characters and truncating to maximum lengths.

### L14: TimeoutLayer::with_status_code argument order

**Session 10.** `tower_http::timeout::TimeoutLayer::with_status_code` takes `(status_code: StatusCode, timeout: Duration)` — status code **first**, duration second. The deprecated `TimeoutLayer::new` takes `(Duration)` only. Always check the signature; the argument order is counter-intuitive relative to the "with_status_code" naming. Swapping them produces E0308 with a "swap these arguments" hint.

### L15: clippy::significant_drop_tightening with MutexGuard + entry API

**Session 10.** The nursery lint `significant_drop_tightening` fires when a `MutexGuard` is held past its last meaningful use. Fix: wrap the entire mutex interaction in an inner block (`{...}`) so the guard drops at the closing brace. For `HashMap::entry()` patterns where `&mut V` borrows the guard: copy the return value into a local (`let fc = entry.val;`) then call `drop(map)` explicitly — NLL ends the entry borrow at its last use, making the explicit drop valid. Never use `drop(&mut T)` — that is a no-op and triggers `clippy::dropping_references`.

### L16: Documentation staleness compounds across sessions

**Session 11.** Test counts drifted across multiple sessions (Sessions 5–10) before being caught each time. The pattern: a session adds tests, updates CLAUDE.md, but misses README.md, CONTRIBUTING.md, or ROADMAP.md. Each uncorrected file becomes a stale reference for future sessions. Fix: immediately after every `cargo test` run, grep all documentation files for the previous count and replace with the verified current count. Do not defer this to the end of the session. A post-session checklist item — `grep -rn "<old_count>" *.md` — catches stragglers before commit.

### L17: Security headers must be verified in integration tests

**Session 11.** OWASP headers (`X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, `Content-Security-Policy`) were added in Session 10 and the integration test `test_security_headers_present` is the only automated enforcement. Without a test, a future refactor of the middleware stack could silently drop a header. Add an integration test for every security invariant at the time the invariant is introduced — not as a follow-up. A security property without a test is an unverified claim.

### L18: Prometheus counters require end-to-end wiring verification

**Session 11.** `mallard_events_ingested_total` was declared as an `AtomicU64` in `AppState` in an earlier session but was not incremented in the ingest handler until Session 10. The counter existed and `/metrics` exposed it, but it was always zero. The pattern: declaring a counter and wiring it to the metrics endpoint is not sufficient — the counter must also be incremented at the actual event boundary. Add an integration test that ingests N events and reads `/metrics`, asserting the counter equals N. Without this test, a non-incremented counter is invisible until a user notices flat graphs in production.

### L19: Blocking I/O inside tokio::spawn starves the async worker pool

**Session 12.** Tokio's async runtime uses a fixed-size thread pool (default: number of CPU cores). Any blocking call inside `tokio::spawn(async { ... })` — including `parking_lot::Mutex::lock()` under contention and DuckDB filesystem I/O — holds an async worker thread for the duration of the block. When a Parquet flush takes 6 seconds (`parquet_flush/1000` in PERF.md), a worker thread is stuck for 6 seconds, starving all HTTP request handling on that thread.

Detection: slow HTTP response latency correlating with flush intervals; blocked worker threads visible in Tokio console.

Fix: use `tokio::task::spawn_blocking` for any operation that may block for more than ~1 ms. This runs the work on a dedicated blocking thread pool (default: 512 threads) that does not affect the async scheduler. The pattern from `shutdown_signal()` in `main.rs` is the template — the periodic flush was missing this wrapper while shutdown used it correctly.

Rule: any `Mutex::lock()` that might wait, any filesystem I/O, and any DuckDB SQL call must be in `spawn_blocking`. Call `spawn_blocking(...).await` from the async side — non-blocking wait.

### L20: std::mem::take before success creates silent data loss

**Session 12.** `std::mem::take(&mut *buf)` atomically drains the in-memory event buffer. If this is called before the DuckDB insert loop and any insert fails (schema mismatch, OOM, corrupt state), the local `Vec<Event>` is dropped and all drained events are permanently lost. The caller receives a `500 Internal Server Error` but the event data is unrecoverable.

Correct pattern: drain atomically (to prevent double-processing by concurrent flushes), attempt inserts, and if any fail, restore the drained events to the front of the buffer before returning `Err`. Events pushed after the drain will be at the back of the buffer; prepend the failed events to preserve them. Only leave the buffer empty when all inserts have succeeded.

Code contract: `flush()` must never silently discard events. If it returns `Err`, all events must either be in the buffer (for retry) or in the DuckDB table (visible via `events_all`). Both are acceptable; disappearing into thin air is not.

### L21: Criterion benchmarks must never put setup code inside b.iter()

**Session 12.** Setup code inside `b.iter()` is measured as part of every iteration. When setup dominates (e.g., DuckDB cold-start at ~500 ms per call), the measurement is invalid and misleading.

Diagnostic signal: near-identical timings across dramatically different input sizes. If inserting 100 events takes 17 ms and inserting 1 000 events takes 19 ms, the measurement is dominated by a fixed cost that dwarfs the variable work. The input size should make a proportional difference.

Correct pattern (steady-state): set up DuckDB connection, schema, and buffer OUTSIDE `b.iter()`. Inside `b.iter()`, measure only the operation under test (push or flush). Reset state at the end of each iteration (e.g., call `buffer.flush()` to empty the buffer, but don't measure it).

Correct pattern (per-iteration state): use `b.iter_batched(setup_fn, bench_fn, BatchSize::SmallInput)`. The `setup_fn` runs once per batch (not measured); `bench_fn` is measured. This is correct for flush benchmarks where each flush consumes state that must be recreated.

The three-run minimum (L9 from duckdb-behavioral) catches fluke measurements. Publish before/after baselines when restructuring benchmarks; always note whether old baselines are being superseded and why.

---

## Inherited Lessons from duckdb-behavioral

These 15 lessons were proven over 16 development sessions of the [duckdb-behavioral](https://github.com/tomtom215/duckdb-behavioral) project:

1. E2E testing is non-negotiable
2. Negative benchmark results are results -- document honestly
3. Measure before committing to "obviously better" data structures
4. Never claim parity/coverage/performance without verification
5. Property-based testing catches algebraic violations
6. Mutation testing reveals test gaps
7. Validate SQL test expectations against real DuckDB output
8. Benchmark at scales that exceed cache hierarchies
9. Run benchmarks 3+ times
10. Every optimization is one atomic commit
11. Pin all third-party GitHub Actions to commit SHAs
12. Combine operations are the dominant cost in DuckDB aggregate functions
13. Presorted detection saves O(n log n)
14. `cargo deny` catches transitive dependency license issues
15. Over-engineering is a defect, not a feature
