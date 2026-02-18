FROM rust:1.93-alpine AS builder

RUN apk add --no-cache build-base linux-headers

WORKDIR /build

# Cache dependency builds by copying manifests first
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs \
    && cargo build --release --target x86_64-unknown-linux-musl 2>/dev/null || true \
    && rm -rf src

# Build the actual application
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/mallard-metrics /mallard-metrics

EXPOSE 8000
VOLUME ["/data"]

ENTRYPOINT ["/mallard-metrics"]
