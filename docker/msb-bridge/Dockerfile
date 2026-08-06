# rust:1.90-trixie = glibc 2.39, matching the debian:trixie-slim runtime.
FROM rust:1.90-trixie AS builder
WORKDIR /build
COPY . .
RUN cargo build --release -p msb-bridge

FROM debian:trixie-slim
COPY --from=builder /build/target/release/msb-bridge /usr/local/bin/msb-bridge
ENTRYPOINT ["/usr/local/bin/msb-bridge"]
