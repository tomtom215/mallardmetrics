FROM rust:1.85-slim AS builder

RUN apt-get update && apt-get install -y \
    musl-tools \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /build
COPY . .

RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/mallard-metrics /mallard-metrics

EXPOSE 8000
VOLUME ["/data"]

ENTRYPOINT ["/mallard-metrics"]
