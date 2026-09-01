# Development Lessons

Lessons learned during Mallard Metrics development, organized by category. Each lesson includes context on when it was discovered and why it matters.

---

## Table of Contents

- [Build and Dependencies](#build-and-dependencies)
- [Testing](#testing)
- [Architecture](#architecture)
- [Security](#security)
- [General Engineering Principles](#general-engineering-principles)

---

## Build and Dependencies

### L1: DuckDB COPY TO does not support parameterized queries

DuckDB's `COPY ... TO` statement cannot use `$1`-style parameterized queries. Values must be interpolated into the SQL string. For internal values (site_id, date from the events table itself), this is safe. For user-provided values, use parameterized queries in the SELECT, not in COPY.

### L2: DuckDB Parquet extension must be bundled

The DuckDB Rust crate requires the `parquet` feature flag (`duckdb = { features = ["bundled", "parquet"] }`) to include Parquet support at compile time. Without it, DuckDB tries to auto-download the extension at runtime, which fails in environments without network access (CI, containers, tests).

### L3: Rust edition 2024 features require toolchain >= 1.85

Transitive dependencies (e.g., `getrandom 0.4.x`) may require edition 2024, which is only supported in Rust 1.85.0+. Pin the MSRV to at least 1.85.0. The project now targets 1.98.0 for let-chains and `Ipv6Addr` const support; `Cargo.toml`'s `rust-version` and `rust-toolchain.toml`'s `channel` must agree, and a CI step fails the build if they drift apart.

### L4: DuckDB bundled compilation uses significant disk space

The `duckdb` crate with `bundled` + `parquet` features produces ~26 GB of build artifacts in debug mode, and each distinct feature or profile combination gets its **own** `target/debug/build/libduckdb-sys-*` directory of about 4.4 GB. A session that changes features, or runs `cargo doc` after `cargo test`, accumulates several of them.

The failure mode is not an error. When the filesystem fills, the C++ compile does not abort — it slows to a crawl, so `cargo doc` looks hung at 0% CPU while actually starving. Check `df -h` before concluding a build is stuck.

Cheapest recovery, in order: delete `target/debug/incremental` (~1.4 GB, costs only rebuild speed), then the stale `libduckdb-sys-*` directories — the current ones are the newest with an `output` file. `cargo clean` is the blunt instrument and forces the whole amalgamation to rebuild.

Four commands in one session accumulated four such directories — `cargo check`, `cargo test`, `cargo build` and `cargo doc` each unify features differently — for about 18 GB. Prefer `cargo test --all-targets` over a separate `cargo check` (it type-checks the same code), and expect `cargo build` and `cargo doc` to want a directory each.

### L22: The `behavioral` extension is version-gated on DuckDB

The community extension is built per DuckDB version. `INSTALL behavioral FROM community` returns 404 for a DuckDB release the extension has not been published for — so the `duckdb` crate version is not a free choice: bumping or pinning it can silently disable every behavioral endpoint. Before changing the crate version, confirm the extension resolves for the DuckDB version it bundles (v1.5.5 and v1.5.1 do; v1.4.1 does not).

---

## Testing

### L5: Test against real DuckDB output, not hand-written expectations

SQL test expectations must be validated against actual DuckDB output. Date formatting, type casting, and NULL handling can differ from expectations.

### L6: DuckDB date formatting varies by context

`CAST(timestamp AS DATE)` returns a Date type whose string representation may vary. Use `STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d')` for consistent string formatting in queries that need to compare dates as strings.

### L7: iPhone User-Agent strings contain "Mac OS X"

iPhone UA strings like `"iPhone; CPU iPhone OS 17_2_1 like Mac OS X"` contain the substring "Mac OS X". Detection logic must check for iPhone/iPad before macOS to avoid misclassification.

### L8: Substring matching for referrer sources has collision risks

`"reddit.com".contains("t.co")` is true because `reddit.com` contains the substring `t.co` at position 5 (`reddi[t.co]m`). Use exact hostname matching (`host == "t.co"`) for short domain names to avoid false positives.

### L23: A test that reads the wall clock tests the clock as well as the code

The realtime tests inserted `NOW() - INTERVAL 'n minutes'` and then queried against the wall clock. An event written at `:59.9` and a minute spine built a millisecond later disagree about which bucket it belongs to, so the suite was one unlucky scheduling gap away from a red build. Worse, wall-clock tests cannot assert the *edges*, which is where the interesting behaviour lives.

Fix: take the instant as a parameter. `query_realtime` reads `Utc::now()` once and delegates to `query_realtime_at(conn, site, window, now)`; the tests call the latter with a fixed timestamp. Boundary inclusivity, future-dated events and per-minute bucketing then become exact assertions rather than approximations, and one test still exercises the wall-clock entry point so it cannot rot.

### L24: A test that does not flush is testing the buffer, not the database

Two buffer tests pushed events and immediately queried the `events` table. `push` only buffers below the flush threshold, so both queried an empty table: one failed with `QueryReturnedNoRows`, the other compared an empty vector against three expected paths.

`flush()` also moves rows onward to Parquet and deletes them from the hot table, so after a flush the only place the data exists is the `events_all` view. Any test asserting on written data must flush first and read through `events_all` — which is stronger anyway, since it covers the Parquet round-trip that the hot table alone would hide.

### L25: A poisoned test mutex turns one failure into a dozen

Tests that mutate the process environment serialise on a `static Mutex<()>`. When one test panics while holding it, every later `lock().unwrap()` panics with `PoisonError` — so a single real assertion failure was reported as twelve failures, eleven of them fictional, with the genuine one buried in the middle.

The lock guards ordering, not invariants, so poisoning carries no information: `ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner)`. The failure list then names exactly what broke.

---

## Architecture

### L9: Behavioral extension availability is runtime-dependent

The `behavioral` extension is installed from the DuckDB community repository at runtime. Unit tests cannot assume it is available. Queries using `sessionize`, `window_funnel`, `retention`, etc. must gracefully handle the extension being absent. Use `unwrap_or(default)` for metrics that depend on behavioral functions.

### L10: E2E testing is non-negotiable

Unit tests alone miss integration boundary bugs. HTTP API integration tests validate the full path: JSON -> handler -> buffer -> DuckDB -> response.

### L26: Do not use SQL clock functions against naive-UTC event data

`NOW()` and `CURRENT_TIMESTAMP` return `TIMESTAMP WITH TIME ZONE`. Two consequences bit this project at once:

1. `TIMESTAMPTZ - INTERVAL` is implemented by DuckDB's **ICU extension**, which this build does not load. Every realtime query failed at bind time with "No function matches the given name and argument types '-(TIMESTAMP WITH TIME ZONE, INTERVAL)'", so the endpoint returned `500` in production.
2. Casting back with `::TIMESTAMP` (or `CURRENT_LOCALTIMESTAMP`) resolves through the session time zone, which defaults to the **host's** locale. Every stored timestamp is naive UTC, so on a server not set to UTC the window silently slides by the UTC offset — no error, just wrong numbers.

Compute the instant in Rust with `Utc::now().naive_utc()` and bind it as a parameter. This also makes the code testable and keeps sub-queries of one request consistent with each other, rather than each calling `NOW()` and possibly straddling a boundary.

### L27: Epoch-aligned buckets do not respect the calendar

`salt_period` numbers rotation windows from the Unix epoch so that every instance sharing a secret agrees on boundaries. That is correct, and it means two dates a few days apart can still land in different periods: 2024-01-15 and 2024-01-20 are five days apart and in *different* 30-day periods, because the boundary falls on the 18th.

A test needing "two dates in the same period" must derive them from a period start, never pick calendar dates by hand. The same arithmetic explains a product behaviour worth documenting rather than hiding: a retention cohort spanning a salt boundary loses its visitor linkage.

### L28: A `UNION ALL` over a live glob has no transaction around it

`events_all` unions the hot DuckDB table with `read_parquet('.../*.parquet')`. DuckDB's MVCC covers the table half; the glob half is the filesystem, re-expanded on every query. Any operation that writes one side before removing the other therefore has a window where a row is visible twice — a flush (copy, then delete), a compaction (rename in, then unlink sources), an erasure (delete rows, then remove partitions).

Reversing the order does not fix it; it swaps double-counting for a window where the data is missing entirely. Neither can be tested by an in-process suite that shares a single connection, because the writer mutex accidentally serialises everything: the bug only appears once reads have their own connections, which is exactly what production does and what the test fixtures did not.

The fix is a reader-writer lock spanning the inconsistent window — coarse, held only across file operations, and worth far more than the throughput it costs. The test that proves it has to build a real file-backed database with a real reader pool; a version driven through the in-memory fixtures passed with or without the lock, which would have been worse than no test at all.

### L29: The default extension directory does not exist in a `FROM scratch` image

DuckDB installs community extensions under `$HOME/.duckdb/extensions`. A `scratch` image has no `/etc/passwd` and sets no `HOME`, and the deployment this project recommends adds `read_only: true`. So `INSTALL behavioral FROM community` had nowhere to write, and every funnel, retention, session, sequence and flow request answered 503 — on a container that passed its healthcheck, served the dashboard, and ingested events perfectly.

Nothing in the test suite could see it: the tests open in-memory databases on a normal filesystem with a normal `HOME`. The general lesson is that a dependency's *default path* is part of the deployment contract. Set it explicitly to somewhere the deployment guarantees is writable — here, the data volume — rather than inheriting a default that happens to work on a developer's laptop.

### L11: Axum Tower middleware composes cleanly

CORS, tracing, and compression are added as Tower layers with no impedance mismatch. The `tower::ServiceExt` trait enables testing routers with `oneshot()` without starting a real server.

---

## Security

### L12: Never interpolate user input into SQL strings

All user-provided values (site_id, dates, event names) must use parameterized queries (`$1`, `?`). The only exceptions are column names from fixed enums and internal values from previous query results.

### L13: Input validation at the boundary

Validate all inbound event data for type, length, and format in the handler before passing to the buffer. Sanitize strings by removing control characters and truncating to maximum lengths.

### L14: TimeoutLayer::with_status_code argument order

`tower_http::timeout::TimeoutLayer::with_status_code` takes `(status_code: StatusCode, timeout: Duration)` — status code **first**, duration second. The deprecated `TimeoutLayer::new` takes `(Duration)` only. Always check the signature; the argument order is counter-intuitive relative to the "with_status_code" naming. Swapping them produces E0308 with a "swap these arguments" hint.

### L15: clippy::significant_drop_tightening with MutexGuard + entry API

The nursery lint `significant_drop_tightening` fires when a `MutexGuard` is held past its last meaningful use. Fix: wrap the entire mutex interaction in an inner block (`{...}`) so the guard drops at the closing brace. For `HashMap::entry()` patterns where `&mut V` borrows the guard: copy the return value into a local (`let fc = entry.val;`) then call `drop(map)` explicitly — NLL ends the entry borrow at its last use, making the explicit drop valid. Never use `drop(&mut T)` — that is a no-op and triggers `clippy::dropping_references`.

### L16: Documentation staleness compounds across sessions

Test counts drifted across multiple sessions (Sessions 5–10) before being caught each time. The pattern: a session adds tests, updates CLAUDE.md, but misses README.md, CONTRIBUTING.md, or ROADMAP.md. Each uncorrected file becomes a stale reference for future sessions. Fix: immediately after every `cargo test` run, grep all documentation files for the previous count and replace with the verified current count. Do not defer this to the end of the session. A post-session checklist item — `grep -rn "<old_count>" *.md` — catches stragglers before commit.

### L17: Security headers must be verified in integration tests

OWASP headers (`X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, `Content-Security-Policy`) were added in a previous iteration and the integration test `test_security_headers_present` is the only automated enforcement. Without a test, a future refactor of the middleware stack could silently drop a header. Add an integration test for every security invariant at the time the invariant is introduced — not as a follow-up. A security property without a test is an unverified claim.

### L18: Prometheus counters require end-to-end wiring verification

`mallard_events_ingested_total` was declared as an `AtomicU64` in `AppState` in an earlier session but was not incremented in the ingest handler until a previous iteration. The counter existed and `/metrics` exposed it, but it was always zero. The pattern: declaring a counter and wiring it to the metrics endpoint is not sufficient — the counter must also be incremented at the actual event boundary. Add an integration test that ingests N events and reads `/metrics`, asserting the counter equals N. Without this test, a non-incremented counter is invisible until a user notices flat graphs in production.

### L19: Blocking I/O inside tokio::spawn starves the async worker pool

Tokio's async runtime uses a fixed-size thread pool (default: number of CPU cores). Any blocking call inside `tokio::spawn(async { ... })` — including `parking_lot::Mutex::lock()` under contention and DuckDB filesystem I/O — holds an async worker thread for the duration of the block. When a Parquet flush takes 6 seconds (`parquet_flush/1000` in PERF.md), a worker thread is stuck for 6 seconds, starving all HTTP request handling on that thread.

Detection: slow HTTP response latency correlating with flush intervals; blocked worker threads visible in Tokio console.

Fix: use `tokio::task::spawn_blocking` for any operation that may block for more than ~1 ms. This runs the work on a dedicated blocking thread pool (default: 512 threads) that does not affect the async scheduler. The pattern from `shutdown_signal()` in `main.rs` is the template — the periodic flush was missing this wrapper while shutdown used it correctly.

Rule: any `Mutex::lock()` that might wait, any filesystem I/O, and any DuckDB SQL call must be in `spawn_blocking`. Call `spawn_blocking(...).await` from the async side — non-blocking wait.

### L20: std::mem::take before success creates silent data loss

`std::mem::take(&mut *buf)` atomically drains the in-memory event buffer. If this is called before the DuckDB insert loop and any insert fails (schema mismatch, OOM, corrupt state), the local `Vec<Event>` is dropped and all drained events are permanently lost. The caller receives a `500 Internal Server Error` but the event data is unrecoverable.

Correct pattern: drain atomically (to prevent double-processing by concurrent flushes), attempt inserts, and if any fail, restore the drained events to the front of the buffer before returning `Err`. Events pushed after the drain will be at the back of the buffer; prepend the failed events to preserve them. Only leave the buffer empty when all inserts have succeeded.

Code contract: `flush()` must never silently discard events. If it returns `Err`, all events must either be in the buffer (for retry) or in the DuckDB table (visible via `events_all`). Both are acceptable; disappearing into thin air is not.

### L21: Criterion benchmarks must never put setup code inside b.iter()

Setup code inside `b.iter()` is measured as part of every iteration. When setup dominates (e.g., DuckDB cold-start at ~500 ms per call), the measurement is invalid and misleading.

Diagnostic signal: near-identical timings across dramatically different input sizes. If inserting 100 events takes 17 ms and inserting 1 000 events takes 19 ms, the measurement is dominated by a fixed cost that dwarfs the variable work. The input size should make a proportional difference.

Correct pattern (steady-state): set up DuckDB connection, schema, and buffer OUTSIDE `b.iter()`. Inside `b.iter()`, measure only the operation under test (push or flush). Reset state at the end of each iteration (e.g., call `buffer.flush()` to empty the buffer, but don't measure it).

Correct pattern (per-iteration state): use `b.iter_batched(setup_fn, bench_fn, BatchSize::SmallInput)`. The `setup_fn` runs once per batch (not measured); `bench_fn` is measured. This is correct for flush benchmarks where each flush consumes state that must be recreated.

The three-run minimum (L9 from duckdb-behavioral) catches fluke measurements. Publish before/after baselines when restructuring benchmarks; always note whether old baselines are being superseded and why.

### L30: State that only lives in memory is a security boundary, not a convenience

The admin password hash sat in a `Mutex<Option<String>>` and nothing wrote it anywhere. Every individual piece was right — Argon2id, a minimum length, per-IP lockout, a `409` on a second setup — and the whole was a takeover: a restart put the instance back into first-run mode, so whoever reached it next could set the password and mint an admin API key that outlived the takeover.

Two habits catch this class. First, ask of any in-memory credential what a restart means; "sessions are cleared" is fine, "anyone can claim the instance" is not. Second, test the restart. The in-process suite cannot, by construction — it builds one `AppState` and drops it — so the check belongs in the end-to-end script that stops the binary and starts it again against the same data directory.

### L31: A shared placeholder turns rate limiting into denial of service

The login handler resolved the client address without the peer socket, so on any deployment not behind a trusted proxy — the default — every attempt keyed the same bucket: the literal string `"unknown"`. Every unit test passed, because each drove one synthetic address and the behaviour under a single key is indistinguishable.

The tell is a fallback value that is *shared* rather than *absent*. `Option::None` would have forced a decision at every call site; `"unknown"` silently merged every client into one, which flipped a protection into an amplification — five failures from anyone locked out everyone.

---

## General Engineering Principles

Foundational lessons validated across multiple Rust + DuckDB projects:

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
