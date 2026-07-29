FROM rust:1.90 AS builder
WORKDIR /build
COPY . .
RUN cargo build --release -p msb-controller
FROM debian:trixie-slim
COPY --from=builder /build/target/release/msb-controller /usr/local/bin/msb-controller
ENTRYPOINT ["/usr/local/bin/msb-controller"]
