# PERF.md — Performance Engineering

## Benchmark Framework

- **Tool**: Criterion.rs 0.5 with HTML reports
- **Methodology**: 100 samples per benchmark, 95% confidence intervals
- **Stability**: Benchmarks run 3+ times before comparing
- **Acceptance**: Improvements accepted only when confidence intervals do not overlap

## Required Benchmarks

| Benchmark | Operation | Scales | Status |
|---|---|---|---|
| `ingest_throughput` | Buffer push (event → buffer) | 100, 1K, 10K | Implemented |
| `parquet_flush` | Buffer flush to Parquet | 1K, 10K | Implemented |
| `query_visitors` | Unique visitors query | 10K, 100K, 1M | Phase 2 |
| `query_bounce_rate` | Bounce rate (sessionize) | 10K, 100K, 1M | Phase 2 |
| `query_funnel` | Conversion funnel (window_funnel) | 10K, 100K, 1M | Phase 3 |
| `query_retention` | Retention cohort (retention) | 10K, 100K, 1M | Phase 3 |
| `query_sequence` | Pattern match (sequence_match) | 10K, 100K, 1M | Phase 3 |
| `query_flow` | Next page (sequence_next_node) | 10K, 100K, 1M | Phase 3 |

## Current Baseline

Not yet measured. Benchmarks are defined in `benches/ingest_bench.rs` and compile successfully. First measurements will be taken and documented here after Phase 1 stabilization.

## Algorithmic Complexity

| Operation | Complexity | Notes |
|---|---|---|
| Event buffer push | O(1) amortized | Vec push with pre-allocated capacity |
| Buffer flush | O(n) | Linear scan of events for partitioning |
| Parquet write | O(n) | DuckDB COPY TO with ZSTD compression |
| Visitor ID hash | O(len(IP) + len(UA)) | HMAC-SHA256, constant-time |
| Daily salt | O(1) | HMAC-SHA256 of fixed-size input |
| Unique visitors | O(n) | COUNT(DISTINCT) scan |
| Bounce rate | O(n log n) | sessionize window function (sort + scan) |

## Performance Claims Format

Every claim must include:
- Input size and characteristics
- Criterion mean with 95% confidence interval
- Throughput (events/sec or queries/sec)
- Environment (rustc version, platform)

No performance claims are made without measurement. "Not yet measured" is the default state.

## Optimization History

No optimizations attempted yet. Phase 1 focuses on correctness, not performance.
