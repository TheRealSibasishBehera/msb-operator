# rust:1.90-trixie = glibc 2.39, matching the debian:trixie-slim runtime below
# (a newer-glibc runtime than builder is the mismatch that bit us before).
FROM rust:1.90-trixie AS builder
# protoc: prost-build compiles the device-plugin v1beta1 .proto at build time.
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN cargo build --release -p msb-daemon

FROM debian:trixie-slim
COPY --from=builder /build/target/release/msb-daemon /usr/local/bin/msb-daemon
ENTRYPOINT ["/usr/local/bin/msb-daemon"]
