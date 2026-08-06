# --- Stage 1: fetch the msb binary from the release bundle --------------------
# The daemon shells out to `msb pull`. Pull is pure userspace, so only the `msb`
# binary is needed — no libkrunfw, no VM libraries.
FROM ubuntu:24.04 AS msb
ARG MSB_VERSION=v0.6.8
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN arch="$(uname -m)"; \
    case "$arch" in \
        aarch64|arm64) msb_arch=aarch64 ;; \
        x86_64|amd64)  msb_arch=x86_64 ;; \
        *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    url="https://github.com/superradcompany/microsandbox/releases/download/${MSB_VERSION}/microsandbox-linux-${msb_arch}.tar.gz"; \
    mkdir -p /extract; \
    curl -fsSL "$url" | tar -xz -C /extract; \
    install -D -m 755 /extract/msb /out/usr/local/bin/msb; \
    /out/usr/local/bin/msb --version

# --- Stage 2: build the daemon ------------------------------------------------
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

# --- Stage 3: the daemon image ------------------------------------------------
FROM debian:trixie-slim
# msb pull fetches over HTTPS, so the bundled binary needs CA certificates.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=msb /out/usr/local/bin/msb /usr/local/bin/msb
COPY --from=builder /build/target/release/msb-daemon /usr/local/bin/msb-daemon
ENTRYPOINT ["/usr/local/bin/msb-daemon"]
