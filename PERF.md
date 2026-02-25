# Performance Engineering

This document describes the benchmark framework, methodology, and performance characteristics of Mallard Metrics.

---

## Table of Contents

- [Benchmark Framework](#benchmark-framework)
- [Benchmark Suite](#benchmark-suite)
- [Algorithmic Complexity](#algorithmic-complexity)
- [Performance Claims Format](#performance-claims-format)
- [Optimization History](#optimization-history)

---

## Benchmark Framework

| Property | Value |
|---|---|
| Tool | Criterion.rs 0.5 with HTML reports |
| Location | `benches/ingest_bench.rs` |
| Sample size | 100 per benchmark |
| Confidence interval | 95% |
| Stability requirement | Run 3+ times before comparing |
| Acceptance criteria | Improvements accepted only when confidence intervals do not overlap |

### Running Benchmarks

```bash
# Full benchmark suite with measurements
cargo bench

# Compilation check only (used in CI)
cargo bench --no-run
```

HTML reports are generated in `target/criterion/` after each run.

---

## Benchmark Suite

| Benchmark | Operation | Scales | Status |
|---|---|---|---|
| `ingest_throughput` | Buffer push (event -> buffer) | 100, 1K, 10K | Implemented |
| `parquet_flush` | Buffer flush to Parquet | 1K, 10K | Implemented |
| `query_core_metrics` | Core metrics query (visitors, pageviews) | -- | Implemented |
| `query_timeseries` | Time-bucketed aggregation | -- | Implemented |
| `query_breakdowns` | Dimension breakdown queries | -- | Implemented |

### Current Baseline

**Environment:**

| Property | Value |
|---|---|
| `rustc` | 1.93.1 (01f6ddf75 2026-02-11) |
| Platform | Linux 4.4.0 x86\_64 |
| CPU | Intel GenuineIntel, 2.1 GHz, 8192 KB L2 cache, 16 cores |
| Memory | 21 GiB |
| Profile | `release` (`lto = true`, `codegen-units = 1`) |
| Sample size | 100 per benchmark (Criterion default) |
| CI level | 95% |

**Measurement methodology:** Each benchmark group was run three times consecutively using the compiled benchmark binary with a group filter. The median run (by mean) was selected as the canonical baseline. All three run means are recorded for reproducibility.

---

#### `ingest_throughput` — Buffer push (event → in-memory buffer)

> **SUPERSEDED — Session 12.** These baselines are invalid. DuckDB connection setup and schema
> initialisation ran INSIDE `b.iter()`, so the ~500 ms cold-start dominated every sample.
> The near-flat scaling from 100 → 1 000 events (17 ms vs 19 ms) is the diagnostic signal:
> the fixed cold-start cost completely masked the variable push cost. The benchmarks have been
> restructured in Session 12 (setup outside `b.iter()`). New steady-state baselines must be
> measured in a future session.

| Benchmark | Run 1 mean | Run 2 mean | Run 3 mean | **Canonical (median run 95% CI)** |
|---|---|---|---|---|
| `ingest_throughput/100` | 17.265 ms | 17.153 ms | 17.544 ms | ~~17.265 ms \[16.964 ms, 17.582 ms\]~~ (**invalid — cold-start**) |
| `ingest_throughput/1000` | 19.794 ms | 20.178 ms | 18.640 ms | ~~19.794 ms \[19.317 ms, 20.264 ms\]~~ (**invalid — cold-start**) |
| `ingest_throughput/10000` | 28.503 ms | 32.340 ms | 29.715 ms | ~~29.715 ms \[28.726 ms, 30.694 ms\]~~ (**invalid — cold-start**) |

---

#### `parquet_flush` — Buffer flush to Parquet file

> **SUPERSEDED — Session 12.** Same cold-start contamination as `ingest_throughput`. Each
> iteration created a fresh DuckDB connection and ran ~1000 row-by-row INSERTs before
> timing the Parquet flush. The 6 s/iter cost was dominated by the row-by-row INSERT loop
> (replaced with DuckDB Appender API in Session 12) and DuckDB cold-start, not by COPY TO Parquet.
> Benchmarks restructured with `iter_batched`; new baselines needed.

| Benchmark | Run 1 (95% CI) | Run 2 | Run 3 | **Canonical** |
|---|---|---|---|---|
| `parquet_flush/1000` | 6.0407 s \[6.0238 s, 6.0600 s\] | — | — | ~~6.04 s~~ (**invalid — cold-start + row-by-row inserts**) |
| `parquet_flush/10000` | — | — | — | Not measured |

---

#### `query_metrics` — Analytics queries over 10K pre-loaded events

DuckDB schema and 10K events are initialized once **outside** the benchmark loop. Timing measures query execution only.

| Benchmark | Run 1 mean | Run 2 mean | Run 3 mean | **Canonical (median run 95% CI)** |
|---|---|---|---|---|
| `core_metrics_10k` | 4.1598 ms | 4.1724 ms | 4.2849 ms | **4.1724 ms \[4.1462 ms, 4.1992 ms\]** |
| `timeseries_10k` | 3.0022 ms | 2.9751 ms | 3.0019 ms | **3.0019 ms \[2.9860 ms, 3.0181 ms\]** |
| `breakdown_pages_10k` | 3.5319 ms | 3.5110 ms | 3.5630 ms | **3.5319 ms \[3.5102 ms, 3.5541 ms\]** |

All three query types complete in under 5 ms over 10K events with CIs under 1.5% width.

---

To generate measurements locally:

```bash
cargo bench
# View results in target/criterion/report/index.html

# Run a specific group only:
./target/release/deps/ingest_bench-<hash> --bench "query_metrics"
./target/release/deps/ingest_bench-<hash> --bench "ingest_throughput"
```

---

## Algorithmic Complexity

| Operation | Complexity | Notes |
|---|---|---|
| Event buffer push | O(1) amortized | `Vec` push with pre-allocated capacity |
| Buffer flush (DuckDB insert) | O(n) | DuckDB Appender API — columnar batch insert, no per-row SQL parse |
| Parquet write | O(n) | DuckDB `COPY TO` with ZSTD compression |
| Next Parquet file path | O(k) | `read_dir` scan of k existing files in partition — one syscall. Was O(k) stat syscalls (one per file); fixed Session 12. |
| Visitor ID hash | O(len(IP) + len(UA)) | HMAC-SHA256, constant-time comparison |
| Daily salt generation | O(1) | HMAC-SHA256 of fixed-size date input |
| Unique visitors query | O(n) | `COUNT(DISTINCT)` scan over partition |
| Bounce rate query | O(n log n) | `sessionize()` window function (sort + scan) |
| Query cache lookup | O(1) | Hash map with TTL expiration |
| Rate limit check | O(1) | Token-bucket per site_id |

---

## Performance Claims Format

Every performance claim must include:

1. **Input size and characteristics** -- Number of events, time range, cardinality
2. **Criterion mean with 95% confidence interval** -- e.g., `1.23 ms [1.21 ms, 1.25 ms]`
3. **Throughput** -- events/sec or queries/sec where applicable
4. **Environment** -- `rustc` version, platform, CPU, memory

**No performance claims are made without measurement.** "Not yet measured" is the default state for any metric not explicitly documented with Criterion output above.

---

## Optimization History

No optimizations with measured results have been documented yet. When optimizations are made, each entry will include:

- **What changed** -- Description of the optimization
- **Before** -- Criterion measurement with CI
- **After** -- Criterion measurement with CI
- **Commit** -- Single-commit reference
- **Verdict** -- Accepted/rejected based on non-overlapping confidence intervals
