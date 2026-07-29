#!/usr/bin/env bash
# End-to-end suite: boots real microVMs, so it needs a KVM cluster with the
# operator installed. The preflight below fails loudly on any missing prereq.
#
#   KUBECONFIG=/path/to/kubeconfig tests/e2e/run.sh
set -euo pipefail

cd "$(dirname "$0")/../.."   # repo root

fail() { echo "PREFLIGHT FAIL: $*" >&2; exit 1; }

echo "== preflight =="

command -v kubectl >/dev/null || fail "kubectl not found"
command -v kubectl-kuttl >/dev/null || command -v kuttl >/dev/null \
  || fail "kubectl-kuttl not found (install from https://kuttl.dev)"

kubectl cluster-info >/dev/null 2>&1 || fail "cannot reach the cluster (check KUBECONFIG)"

kvm=$(kubectl get nodes -o jsonpath='{range .items[*]}{.status.allocatable.devices\.microsandbox\.dev/kvm}{"\n"}{end}' 2>/dev/null | grep -vE '^$|^0$' | head -1 || true)
[ -n "$kvm" ] || fail "no node advertises devices.microsandbox.dev/kvm — is the daemon installed and /dev/kvm present?"
echo "  KVM advertised: $kvm"

kubectl get deploy -A 2>/dev/null | grep -q msb-controller || fail "msb-controller Deployment not found — install the operator first"
echo "  operator: found msb-controller"

kubectl get crd sandboxes.sandbox.microsandbox.dev >/dev/null 2>&1 || fail "Sandbox CRD not installed"
echo "  CRD: installed"

echo "== running e2e suite (kuttl) =="
kuttl_bin=$(command -v kubectl-kuttl || command -v kuttl)
"$kuttl_bin" test --config tests/e2e/kuttl-test.yaml

echo "== e2e suite PASSED =="
