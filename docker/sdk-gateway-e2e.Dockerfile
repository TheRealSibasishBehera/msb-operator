# The SDK-gateway e2e driver, built into a container so it runs in-cluster.

# rust:1.91: the SDK's smoltcp needs it, and trixie glibc matches the final image.
FROM rust:1.91 AS build
# The prebuilt feature links libkrun, which needs libcap-ng / libseccomp / libbz2.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libcap-ng-dev libseccomp-dev libbz2-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /op
COPY tests/sdk-gateway/ ./
RUN cargo build --release \
    && install -D -m 755 target/release/sdk-gateway-e2e /out/usr/local/bin/sdk-gateway-e2e

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates libcap-ng0 libseccomp2 libbz2-1.0 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /
ENTRYPOINT ["/usr/local/bin/sdk-gateway-e2e"]
