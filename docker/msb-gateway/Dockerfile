FROM rust:1.90 AS builder
WORKDIR /build
COPY . .
RUN cargo build --release -p msb-gateway
FROM debian:trixie-slim
COPY --from=builder /build/target/release/msb-gateway /usr/local/bin/msb-gateway
ENTRYPOINT ["/usr/local/bin/msb-gateway"]
