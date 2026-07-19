#!/usr/bin/env bash
# Build a pre-baked microsandbox-cache OCI image from an ordinary app image.
#
#   ./build.sh <app-image> [--prefix <registry/repo>] [--platform <p>] [--tag <override>]
#   ./build.sh python:3.12 --prefix registry.example.com/msb-cache
#
# By default the output tag is DERIVED from the app image by the same convention
# the controller uses (`<prefix>/<slug>-<hash>`), so the built image lands where
# the controller expects to pull it. Pass --tag to override.
#
# The cache is architecture-specific (ADR 0003): the baked EROFS holds the app
# image's rootfs for one arch. `--platform` defaults to the host arch; set it
# explicitly (e.g. linux/amd64) for a KVM node of a different arch — but it must
# be built ON that arch, since msb picks the arch from the build host.
set -euo pipefail

app_image="${1:-}"
shift || true
prefix=""
platform=""
tag_override=""
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)   prefix="$2"; shift 2 ;;
        --platform) platform="$2"; shift 2 ;;
        --tag)      tag_override="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [ -z "$app_image" ]; then
    echo "usage: $0 <app-image> [--prefix <registry/repo>] [--platform <p>] [--tag <override>]" >&2
    exit 2
fi

# Derive the cache tag from the app image. MUST match `derive_cache_ref` in
# crates/msb-controller/src/config.rs: slug = last path segment, lowercased,
# non-[a-z0-9-] -> '-', collapsed and trimmed, capped at 40; hash = first 12 hex
# of sha256(app_image); result = <prefix>/<slug>-<hash>.
derive_cache_ref() {
    local ref="$1" pfx="$2" last slug hash
    last="${ref##*/}"
    slug="$(printf '%s' "$last" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9-]/-/g; s/--*/-/g; s/^-//; s/-$//' | cut -c1-40)"
    hash="$(printf '%s' "$ref" | sha256sum | cut -c1-12)"
    if [ -n "$pfx" ]; then
        printf '%s/%s-%s' "${pfx%/}" "$slug" "$hash"
    else
        printf '%s-%s' "$slug" "$hash"
    fi
}

output_tag="${tag_override:-$(derive_cache_ref "$app_image" "$prefix")}"

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
