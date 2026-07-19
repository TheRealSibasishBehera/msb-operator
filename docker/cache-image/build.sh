#!/usr/bin/env bash
# Build a pre-baked microsandbox-cache OCI image from an ordinary app image.
#
#   ./build.sh <app-image> <output-tag> [platform]
#   ./build.sh python:3.12 registry.example.com/msb-images/python-3.12:latest
#
# The cache is architecture-specific (ADR 0003): the baked EROFS holds the app
# image's rootfs for one arch. `platform` defaults to the host arch; set it
# explicitly (e.g. linux/amd64) to build for a KVM node of a different arch —
# but note it must be built ON that arch, since msb picks the arch from the
# build host, not the container's declared platform.
set -euo pipefail

app_image="${1:-}"
output_tag="${2:-}"
platform="${3:-}"

if [ -z "$app_image" ] || [ -z "$output_tag" ]; then
    echo "usage: $0 <app-image> <output-tag> [platform]" >&2
    exit 2
fi

here="$(cd "$(dirname "$0")" && pwd)"

args=(build
    --build-arg "APP_IMAGE=$app_image"
    -f "$here/Dockerfile"
    -t "$output_tag")
[ -n "$platform" ] && args+=(--platform "$platform")

# Build context is minimal — the Dockerfile pulls everything itself.
echo "building cache image for $app_image -> $output_tag" >&2
docker "${args[@]}" "$here"

echo "built $output_tag" >&2
echo "push with: docker push $output_tag" >&2
