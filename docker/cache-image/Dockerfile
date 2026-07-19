# Builds a pre-baked microsandbox-cache OCI image. Given an ordinary app image,
# runs `msb pull` at build time to materialise the EROFS/VMDK cache, then bakes
# it so a sandbox boots from it at runtime with no pull or conversion.
#
# Usage: docker build --build-arg APP_IMAGE=python:3.12 -f Dockerfile -t <tag> .
# The cache artifacts sit at the image ROOT (layers/, fsmeta/, vmdk/, manifests/).
# Mount it as an image volume at /msb/cache with MSB_HOME=/msb, and they appear
# at /msb/cache/{layers,...} — matching the VMDK's baked extent paths.
#
# ARCH LOCK-IN: the baked cache is architecture-specific (arm64 build -> arm64
# nodes only). Build on / for the same arch as the target KVM nodes.
#
# MSB_HOME PINNING: msb's VMDK descriptor bakes ABSOLUTE extent paths under
# /msb/cache, with no rebase option, so build-time and runtime MSB_HOME must both
# be exactly /msb. /msb must not be a symlink (msb canonicalizes it at pull time).

# ubuntu:24.04 = glibc 2.39, matching the msb release binary (built on
# ubuntu-latest). A lower glibc base would fail to run msb.
FROM ubuntu:24.04 AS builder

ARG APP_IMAGE
# Pinned for reproducibility; bump deliberately after re-checking the contract.
ARG MSB_VERSION=v0.6.6

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Extract only the `msb` binary from the release bundle. We deliberately skip
# the install script's libkrunfw step — pull is pure userspace and needs no VM
# library, and that step is brittle in a minimal container.
RUN arch="$(uname -m)"; \
    case "$arch" in \
        aarch64|arm64) msb_arch=aarch64 ;; \
        x86_64|amd64)  msb_arch=x86_64 ;; \
        *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    url="https://github.com/superradcompany/microsandbox/releases/download/${MSB_VERSION}/microsandbox-linux-${msb_arch}.tar.gz"; \
    curl -fsSL "$url" -o /tmp/msb.tar.gz \
    && tar -xzf /tmp/msb.tar.gz -C /tmp msb \
    && install -m 755 /tmp/msb /usr/local/bin/msb \
    && rm /tmp/msb.tar.gz /tmp/msb \
    && msb --version

ENV MSB_HOME=/msb
RUN test -n "$APP_IMAGE" || { echo "APP_IMAGE build-arg is required" >&2; exit 1; } \
    && msb pull "$APP_IMAGE"

# Keep only what the boot-time cache-hit check requires (layers/fsmeta/vmdk/
# manifests); drop tmp/ staging and .lock files.
RUN find /msb/cache/tmp -mindepth 1 -delete 2>/dev/null || true \
    && find /msb/cache -name '*.lock' -delete \
    && test -n "$(ls -A /msb/cache/vmdk/*.vmdk 2>/dev/null)" \
    && test -n "$(ls -A /msb/cache/manifests/*.json 2>/dev/null)"

# The final image IS the cache, with the artifacts at the image ROOT. An image
# volume mounts the image's whole filesystem at the mountPath, so putting
# layers/fsmeta/vmdk/manifests at `/` makes them appear directly under the
# `/msb/cache` mount — matching the VMDK's baked `/msb/cache/...` extent paths.
# (Copying to `/msb/cache` here would nest them at `/msb/cache/msb/cache/...`.)
# scratch keeps the image to exactly the baked bytes.
FROM scratch
COPY --from=builder /msb/cache/ /
