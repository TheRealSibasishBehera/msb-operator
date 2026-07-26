# SDK ↔ gateway e2e

Proves the contract the gateway exists to keep: the **unmodified** upstream
microsandbox SDK drives the cluster with only `MSB_API_URL` + `MSB_API_KEY` set —
create → exec-in-guest → stop. `src/main.rs` is the test; its exit code is the
result.

## Not a workspace member

It depends on the real published `microsandbox`, whose `microsandbox-types` would
collide with the workspace's own pin. So it's standalone and built on its own,
not via `cargo build --workspace`. Keep its `microsandbox` pin equal to the
workspace pin.

## Not kuttl

kuttl drives declarative YAML at the API server; this is the SDK speaking
CBOR-over-WebSocket to the gateway. There's nothing to `kubectl apply`, so this
is a client binary rather than a kuttl suite.

## Running

Needs a live gateway + KVM cluster; not hosted CI.

```sh
MSB_API_URL=https://<gateway>:8080 \
MSB_API_KEY=$(kubectl create token <sa> -n <ns>) \
  tests/sdk-gateway/run.sh
```

Minimal scope for now: the happy path only. Deferred follow-ups: typed errors,
port/secret rejection, negative-exit/stderr.
