# Changelog

All notable changes to Mallard Metrics will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Project initialization with Rust 1.85.0, Axum 0.8, DuckDB 1.4.4
- Event ingestion endpoint (POST /api/event) with privacy-safe visitor ID
- In-memory event buffer with configurable flush threshold and periodic timer
- Date-partitioned Parquet storage (site_id + date partitioning)
- DuckDB schema with 25-column events table
- Schema migration system with version tracking
- Core metrics queries: unique visitors, total pageviews, bounce rate (sessionize)
- Dimension breakdowns: pages, referrer sources, countries, browsers, OS, devices
- Time-series aggregation with hourly and daily granularity
- Behavioral analytics query builders: funnel (window_funnel), retention, sessions (sessionize), sequences (sequence_match/count), flow (sequence_next_node)
- Dashboard SPA (Preact + HTM, embedded in binary via rust-embed)
- Tracking script (<1KB minified JavaScript)
- Health check endpoint (GET /health)
- CORS support via tower-http
- User-Agent parsing (browser, OS, version detection)
- Referrer source detection (Google, Bing, Twitter, Facebook, etc.)
- UTM parameter extraction
- Input validation and sanitization
- CI pipeline with 11 jobs (build, test, clippy, fmt, docs, MSRV, bench, security, coverage, docker, cross-compile)
- Criterion.rs benchmark suite for ingestion throughput and Parquet flush
- 111 tests (104 unit + 7 integration)
- Dockerfile (multi-stage, FROM scratch)
- docker-compose.yml
