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
