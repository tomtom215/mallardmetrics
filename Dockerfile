FROM rust:1.93-alpine AS builder

RUN apk add --no-cache build-base linux-headers

WORKDIR /build
COPY . .

RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/mallard-metrics /mallard-metrics

EXPOSE 8000
VOLUME ["/data"]

ENTRYPOINT ["/mallard-metrics"]
