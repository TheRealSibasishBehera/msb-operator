#!/usr/bin/env bash
# SDK<->gateway e2e: the unmodified microsandbox SDK drives the cluster. Needs a
# live gateway + KVM cluster; not hosted CI.
#
#   MSB_API_URL=https://<gateway>:8080 \
#   MSB_API_KEY=$(kubectl create token <sa> -n <ns>) \
#     tests/sdk-gateway/run.sh
set -euo pipefail
cd "$(dirname "$0")"

fail() { echo "PREFLIGHT FAIL: $*" >&2; exit 1; }

[ -n "${MSB_API_URL:-}" ] || fail "MSB_API_URL unset"
[ -n "${MSB_API_KEY:-}" ] || fail "MSB_API_KEY unset"
command -v cargo >/dev/null || fail "cargo not found (need Rust >= 1.85 for edition 2024)"
curl -fsS "${MSB_API_URL%/}/healthz" >/dev/null 2>&1 || fail "gateway ${MSB_API_URL} not reachable"

# The microsandbox crate's prebuilt build script downloads libkrunfw — allow time.
cargo run --quiet --release
