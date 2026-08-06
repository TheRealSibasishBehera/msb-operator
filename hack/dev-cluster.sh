#!/usr/bin/env bash
# Stand up a local KVM dev cluster for the e2e suite. Needs a host with /dev/kvm
# (bare metal or nested virt), plus docker, kind, and kubectl.
#
#   hack/dev-cluster.sh create      # kind cluster with /dev/kvm mounted
#   hack/dev-cluster.sh build-load  # build the operator images and load them into the cluster
#   hack/dev-cluster.sh apply       # apply the operator at the loaded images, wait ready
#   hack/dev-cluster.sh delete      # tear the cluster down
#
# Overridable: CLUSTER_NAME, VERSION, IMAGE_REGISTRY, OPERATOR_NS.
set -euo pipefail

cd "$(dirname "$0")/.."   # repo root

CLUSTER_NAME="${CLUSTER_NAME:-msb-dev}"
IMAGE_REGISTRY="${IMAGE_REGISTRY:-msb}"
VERSION="${VERSION:-$(git rev-parse --short HEAD 2>/dev/null || echo dev)}"
OPERATOR_NS="${OPERATOR_NS:-msb-system}"
IMAGES=(msb-controller msb-daemon msb-bridge msb-runtime)

fail() { echo "FAIL: $*" >&2; exit 1; }

# Low fs.inotify.max_user_instances exhausts under kind and crash-loops the daemon
# on EMFILE. https://kind.sigs.k8s.io/docs/user/known-issues/#pod-errors-due-to-too-many-open-files
INOTIFY_MIN=256
check_inotify() {
  local cur
  cur=$(sysctl -n fs.inotify.max_user_instances 2>/dev/null || echo 0)
  [ "$cur" -ge "$INOTIFY_MIN" ] && return
  echo "WARNING: fs.inotify.max_user_instances=$cur is low for kind; if the daemon
  crash-loops with EMFILE ('Too many open files'), raise it:
    sudo sysctl -w fs.inotify.max_user_instances=512 fs.inotify.max_user_watches=1048576
  (persist in /etc/sysctl.d/)" >&2
}

create() {
  command -v kind >/dev/null || fail "kind not found"
  [ -e /dev/kvm ] || fail "/dev/kvm missing — this host cannot run microVMs (need bare metal or nested virt)"
  check_inotify
  if kind get clusters 2>/dev/null | grep -qx "$CLUSTER_NAME"; then
    echo "cluster $CLUSTER_NAME exists, skipping create"
  else
    kind create cluster --name "$CLUSTER_NAME" --config deploy/kind-kvm.yaml
  fi
}

build_load() {
  for img in "${IMAGES[@]}"; do
    docker build -t "$IMAGE_REGISTRY/$img:$VERSION" -f "docker/$img/Dockerfile" .
    kind load docker-image --name "$CLUSTER_NAME" "$IMAGE_REGISTRY/$img:$VERSION"
  done
}

apply() {
  kubectl apply -k deploy/
  # Repoint deploy/'s published image tags at the locally built+loaded ones.
  kubectl -n "$OPERATOR_NS" set image deploy/msb-controller \
    "msb-controller=$IMAGE_REGISTRY/msb-controller:$VERSION"
  kubectl -n "$OPERATOR_NS" set env deploy/msb-controller \
    "MSB_RUNTIME_IMAGE=$IMAGE_REGISTRY/msb-runtime:$VERSION" \
    "MSB_BRIDGE_IMAGE=$IMAGE_REGISTRY/msb-bridge:$VERSION"
  kubectl -n "$OPERATOR_NS" set image ds/msb-daemon "*=$IMAGE_REGISTRY/msb-daemon:$VERSION"
  kubectl -n "$OPERATOR_NS" rollout status deploy/msb-controller --timeout=180s
  kubectl -n "$OPERATOR_NS" rollout status ds/msb-daemon --timeout=180s
  # The device plugin advertises KVM a few seconds after the daemon is Ready.
  for _ in $(seq 1 24); do
    kvm=$(kubectl get nodes -o jsonpath='{.items[0].status.allocatable.devices\.microsandbox\.dev/kvm}' 2>/dev/null || true)
    [ -n "$kvm" ] && [ "$kvm" != "0" ] && { echo "KVM advertised: $kvm"; return; }
    sleep 5
  done
  fail "daemon is up but no node advertises devices.microsandbox.dev/kvm — check: kubectl -n $OPERATOR_NS logs ds/msb-daemon"
}

case "${1:-}" in
  create)     create ;;
  build-load) build_load ;;
  apply)      apply ;;
  delete)     kind delete cluster --name "$CLUSTER_NAME" ;;
  *) fail "usage: $0 create|build-load|apply|delete" ;;
esac
