FROM rust:1.82 AS builder
WORKDIR /build
COPY . .
RUN cargo build --release -p msb-daemon

FROM debian:bookworm-slim
COPY --from=builder /build/target/release/msb-daemon /usr/local/bin/msb-daemon
ENTRYPOINT ["/usr/local/bin/msb-daemon"]
