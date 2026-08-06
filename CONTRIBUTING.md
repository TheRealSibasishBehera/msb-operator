# Contributing

Thanks for your interest in msb-operator. This guide covers how to build, test, and submit changes.

## Development environment

The workspace is Rust and builds with a standard toolchain. Cluster-level work needs `kubectl`, `kind`, and `docker`. The end-to-end suite boots real microVMs, so it needs a host with hardware virtualization (`/dev/kvm`).

Build, lint, and unit-test the workspace:

```sh
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all
```

Stand up a local KVM cluster and run the end-to-end suite:

```sh
make run                 # kind cluster (mounts /dev/kvm), build and load images, deploy
make test SUITE=e2e      # the KVM kuttl suite
```

Run `make help` for the full target list.

## Before opening a pull request

CI runs the checks below, so run them locally first and fix anything they report:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all`

If you changed the CRD types, RBAC, or generated API docs, regenerate the committed artifacts and include the result:

```sh
cargo run -p xtask -- generate-crd
cargo run -p xtask -- generate-rbac
cargo run -p xtask -- generate-api-docs
```

A stale generated file fails CI.

## Pull requests

- **Keep them focused.** One logical change per pull request. Submit cosmetic fixes (typos, formatting) separately from behavior changes.
- **Explain the why.** The code shows how; the pull request body and commit messages should explain why the change is needed and what alternatives you considered.
- **Cover new behavior with tests.** Most code changes should add or update a unit test, a kuttl case, or both. If a change is genuinely untestable, say so in the body.
- **Comment the non-obvious.** Add a comment only where intent is not clear from the code itself, such as a kernel constraint, a protocol quirk, or a Kubernetes behavior that contradicts intuition.

## Reporting issues

When filing a bug, include the operator version, the Kubernetes version, the `Sandbox` manifest that reproduces it, and the relevant controller or daemon logs (`kubectl -n <namespace> logs deploy/msb-controller`).
