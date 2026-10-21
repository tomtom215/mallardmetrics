# syntax=docker/dockerfile:1
#
# Multi-stage build producing a small glibc runtime image.
#
# Why glibc and not a static musl binary in FROM scratch (the previous design):
# the DuckDB `behavioral` community extension is only published for glibc
# platforms. DuckDB derives its platform string from the C library it was
# compiled against, so a musl build asks for `linux_amd64_musl`, which the
# community repository answers with 404. Every funnel, retention, session,
# sequence and flow request then returned 503 on an image that passed its
# healthcheck and ingested events perfectly (see LESSONS.md, L29).
#
#   builder : rust:1.98.0-bookworm   (glibc 2.36, gcc 12)
#   runtime : distroless cc-debian12 (same glibc, libgcc and libstdc++, CA roots,
#             tzdata, no shell, UID 65532)
#
# The builder and the runtime must stay on the same Debian release: a binary
# linked against a newer glibc than the runtime ships fails at start with
# "version `GLIBC_2.xx' not found". scripts/check-binary-abi.sh enforces that
# at build time.
#
# The toolchain version is the tag of the builder image, and must equal the
# channel in rust-toolchain.toml (CI's static-checks job compares the two).
# rust-toolchain.toml itself is excluded from the build context (.dockerignore)
# so rustup does not try to download extra targets inside the build.
#
# Release images are assembled from pre-compiled binaries by
# .github/workflows/release.yml; this Dockerfile is the from-source path used by
# `docker compose build` and by CI's Docker smoke test.

FROM rust:1.99.0-bookworm AS builder

WORKDIR /build

# Warm the dependency cache before copying sources so that editing application
# code does not invalidate the (very slow) bundled-DuckDB compile.
#
# The placeholder tree must cover every target declared in Cargo.toml, and the
# `include_str!`-ed tracking script, or this build fails.
COPY Cargo.toml Cargo.lock ./
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

# Fail the build, not the first start, if the binary needs anything the runtime
# image does not provide. 2.36 is Debian 12's glibc; 3.4.30 its libstdc++.
#
# /rootfs-data is the skeleton of the data directory. It must exist in the image
# and belong to the runtime uid: a named volume (docker-compose.yml) copies the
# ownership of the image directory on first use, so without it the volume would
# be created root-owned and the non-root process could not write to it.
RUN scripts/check-binary-abi.sh target/release/mallard-metrics 2.36 3.4.30 && \
    mkdir -p /rootfs-data

# ─────────────────────────────────────────────────────────────────────────────
# Runtime. `:nonroot` runs as UID/GID 65532 and has no shell or package manager.
# ─────────────────────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12:nonroot

LABEL org.opencontainers.image.title="Mallard Metrics" \
      org.opencontainers.image.description="Self-hosted, privacy-focused web analytics powered by DuckDB and the behavioral extension" \
      org.opencontainers.image.source="https://github.com/tomtom215/mallardmetrics" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

COPY --from=builder --chown=65532:65532 /rootfs-data /data
COPY --from=builder /build/target/release/mallard-metrics /mallard-metrics

USER 65532:65532
WORKDIR /data

# HOME and the extension directory both point at the data volume. DuckDB would
# otherwise install community extensions under $HOME/.duckdb, which in this
# image is /home/nonroot on a read-only root filesystem, so
# `INSTALL behavioral FROM community` would fail and every behavioral endpoint
# would answer 503 (LESSONS.md, L29). The volume is the one path the deployment
# guarantees to be writable.
ENV HOME=/data \
    MALLARD_DATA_DIR=/data \
    MALLARD_EXTENSION_DIR=/data/extensions
EXPOSE 8000
VOLUME ["/data"]

# The image has no shell, curl or wget, so the binary probes itself. Exec form
# is required for the same reason. Declaring it here rather than only in
# docker-compose.yml means `docker run` and Swarm get the check too.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/mallard-metrics", "--healthcheck"]

ENTRYPOINT ["/mallard-metrics"]
