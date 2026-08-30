# syntax=docker/dockerfile:1
#
# Multi-stage build producing a static, scratch-based image.
#
# The builder is the Alpine Rust image, whose host target is already
# *-unknown-linux-musl, so `cargo build --release` produces a fully static
# binary with no cross-compilation. Building for a foreign architecture works
# through buildx/QEMU emulation (`docker build --platform linux/arm64 .`).
#
# Release images are assembled from pre-compiled binaries by
# .github/workflows/release.yml; this Dockerfile is the from-source path used by
# `docker compose build` and by CI's Docker smoke test.

FROM rust:1.98-alpine AS builder

# build-base and linux-headers are required by libduckdb-sys, which compiles the
# bundled DuckDB C++ amalgamation via cc-rs.
RUN apk add --no-cache build-base linux-headers

WORKDIR /build

# Warm the dependency cache before copying sources so that editing application
# code does not invalidate the (very slow) bundled-DuckDB compile.
#
# The placeholder tree must cover every target declared in Cargo.toml, and the
# `include_str!`-ed tracking script, or this build fails.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir -p src benches tracking && \
    echo "fn main() {}" > src/main.rs && \
    : > src/lib.rs && \
    echo "fn main() {}" > benches/ingest_bench.rs && \
    : > tracking/script.js && \
    cargo build --locked --release && \
    rm -rf src benches tracking

COPY . .

# Touch the crate roots so cargo rebuilds them instead of reusing the
# fingerprints of the placeholder files above.
RUN touch src/main.rs src/lib.rs && \
    cargo build --locked --release

FROM scratch

LABEL org.opencontainers.image.title="Mallard Metrics" \
      org.opencontainers.image.description="Self-hosted, privacy-focused web analytics powered by DuckDB and the behavioral extension" \
      org.opencontainers.image.source="https://github.com/tomtom215/mallardmetrics" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

COPY --from=builder /build/target/release/mallard-metrics /mallard-metrics

# Run as a non-root uid. `scratch` has no /etc/passwd, so a numeric uid:gid is
# required; the /data volume must be writable by this uid on the host.
USER 65532:65532

ENV MALLARD_DATA_DIR=/data
EXPOSE 8000
VOLUME ["/data"]

# `scratch` has no shell, curl or wget, so the binary probes itself. Exec form
# is required for the same reason. Declaring it here rather than only in
# docker-compose.yml means `docker run` and Swarm get the check too.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/mallard-metrics", "--healthcheck"]

ENTRYPOINT ["/mallard-metrics"]
