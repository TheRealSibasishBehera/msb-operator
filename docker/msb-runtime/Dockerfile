# The msb-runtime image: the SDK-linked Rust entrypoint plus the baked msb VMM
# binary and libkrunfw. The entrypoint links the msb SDK, which spawns the baked
# `msb` as the VMM child.
#
# Build for the SAME arch as the target KVM nodes (the VMM is arch-specific),
# and ON that arch (the release binary and our compiled binary are native).

# --- Stage 1: fetch the msb VMM binary + libkrunfw from the release bundle -----
# ubuntu:24.04 = glibc 2.39, matching the msb release binary (built on
# ubuntu-latest) and our Rust build stage below.
FROM ubuntu:24.04 AS vmm

# Pinned; bump deliberately after re-checking the contract.
ARG MSB_VERSION=v0.6.8

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Extract msb + libkrunfw from the release bundle. We do NOT run the install
# script — its libkrunfw step is brittle in a minimal container, and the
# libkrunfw filename version is release-specific, so glob it rather than hardcode.
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
    libk="$(ls /extract/libkrunfw.so.* | head -1)"; \
    test -n "$libk" || { echo "libkrunfw not found in bundle" >&2; exit 1; }; \
    install -D -m 644 "$libk" /out/usr/local/lib/libkrunfw.so; \
    /out/usr/local/bin/msb --version

# --- Stage 2: compile the SDK-linked runtime binary ----------------------------
# rust:1.91 (trixie, glibc 2.39 — matches the final image). The msb SDK pulls
# smoltcp (needs rustc >= 1.91) and links libcap-ng / libseccomp natively.
FROM rust:1.91 AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libcap-ng-dev libseccomp-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /op
COPY . .
RUN cargo build --release -p msb-runtime \
    && install -D -m 755 target/release/msb-runtime /out/usr/local/bin/msb-runtime

# --- Stage 3: the runtime image ------------------------------------------------
FROM ubuntu:24.04
# Runtime shared libs the compiled binary dynamically links.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates libcap-ng0 libseccomp2 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=vmm /out/ /
COPY --from=build /out/ /

# The SDK resolves the VMM + libkrunfw from these; the entrypoint also passes
# them explicitly via flags/env, but set them so any msb self-resolution agrees.
ENV MSB_PATH=/usr/local/bin/msb \
    MSB_LIBKRUNFW_PATH=/usr/local/lib/libkrunfw.so \
    MSB_HOME=/msb
ENTRYPOINT ["/usr/local/bin/msb-runtime"]
