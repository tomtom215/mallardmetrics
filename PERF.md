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

Not yet published. Benchmarks compile and run successfully. First formal measurements with full Criterion reports will be published here once a stable baseline is established.

To generate measurements locally:

```bash
cargo bench
# View results in target/criterion/report/index.html
```

---

## Algorithmic Complexity

| Operation | Complexity | Notes |
|---|---|---|
| Event buffer push | O(1) amortized | `Vec` push with pre-allocated capacity |
| Buffer flush | O(n) | Linear scan of events for partitioning by site_id + date |
| Parquet write | O(n) | DuckDB `COPY TO` with ZSTD compression |
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
