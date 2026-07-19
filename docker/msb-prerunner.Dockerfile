FROM rust:1.90 AS builder
WORKDIR /build
COPY . .
RUN cargo build --release -p msb-prerunner

FROM debian:trixie-slim
COPY --from=builder /build/target/release/msb-prerunner /usr/local/bin/msb-prerunner
ENTRYPOINT ["/usr/local/bin/msb-prerunner"]
