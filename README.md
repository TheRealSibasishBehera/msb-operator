# msb-operator

A virtualization add-on for Kubernetes that gives your cluster a secure, isolated execution layer built on lightweight [microsandbox](https://github.com/superradcompany/microsandbox) microVMs. Workloads run as a `Sandbox` custom resource, built on native Kubernetes primitives and managed through the Kubernetes API.

> [!NOTE]
> msb-operator is a work in progress. Its API is `v1alpha1` and may change without prior notice.

## Components

- `msb-controller` reconciles each `Sandbox` into a Pod and Service and tracks its lifecycle (phase, conditions, restarts, expiry).
- `msb-daemon` is a per-node DaemonSet. It advertises `/dev/kvm` to the kubelet as the `devices.microsandbox.dev/kvm` extended resource, so sandbox Pods schedule onto KVM-capable nodes.
- `msb-runtime` is the per-Pod container that boots the microVM, supervises it, and exits with its status.
- `msb-bridge` is a sidecar that exposes the sandbox's agent socket over WebSocket for in-cluster access.

An optional `msb-gateway` impersonates the microsandbox cloud API, so the unmodified microsandbox SDK can drive the cluster. The `Sandbox` CRD is the primary interface; the gateway is a compatibility layer on top of it.

## Requirements

- A Kubernetes cluster whose sandbox nodes have hardware virtualization. `/dev/kvm` must exist on those nodes (bare metal or nested virtualization).

## Install

### Operator

Each release publishes rendered, image-pinned manifests as assets. Apply the operator:

```sh
kubectl apply -f https://github.com/TheRealSibasishBehera/msb-operator/releases/latest/download/msb-operator.yaml
```

### Gateway (optional)

Install the gateway to also drive the cluster with the microsandbox SDK (see [SDK access via the gateway](#sdk-access-via-the-gateway)):

```sh
kubectl apply -f https://github.com/TheRealSibasishBehera/msb-operator/releases/latest/download/msb-operator-gateway.yaml
```

### From a checkout

To build and run from source, use `make run`, which builds the operator images and deploys them to a local KVM cluster. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Getting started

Run code in a sandbox by applying a `Sandbox` resource:

```yaml
apiVersion: sandbox.microsandbox.dev/v1alpha1
kind: Sandbox
metadata:
  name: my-sandbox
spec:
  image: "python"
  cpus: 1
  memory: 512
  cmd: ["python", "-c", "print('Hello from a microVM!')"]
  runPolicy: Once
```

### SDK access via the gateway

Once the [optional gateway](#install) is installed, the standard microsandbox SDK drives the cluster unchanged. Point it at the gateway and authenticate with a Kubernetes ServiceAccount token; the gateway derives your namespace from that token, so your sandboxes stay scoped to it:

```sh
export MSB_API_URL="http://msb-gateway.msb-system.svc:8080"
export MSB_API_KEY="$(kubectl -n <your-namespace> create token <serviceaccount>)"
```

For local development, reach it from your machine with a port-forward (`export MSB_API_URL="http://localhost:8080"`):

```sh
kubectl -n msb-system port-forward svc/msb-gateway 8080:8080
```

Either way, use the SDK exactly as you would against microsandbox itself. In Rust, add it with `cargo add microsandbox` and point it at the gateway with `set_default_backend(CloudBackend::from_env())`, which reads `MSB_API_URL` and `MSB_API_KEY` from the environment set above:

```rust
use microsandbox::{CloudBackend, Sandbox, set_default_backend};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    set_default_backend(CloudBackend::from_env()?);

    let sandbox = Sandbox::builder("my-sandbox")
        .image("python")
        .cpus(1)
        .memory(512)
        .create()
        .await?;

    let output = sandbox
        .exec("python", ["-c", "print('Hello from a microVM!')"])
        .await?;

    println!("{}", output.stdout()?);

    sandbox.stop().await?;

    Ok(())
}
```

The Python, TypeScript, and Go SDKs work the same way against the gateway; see the [microsandbox SDK reference](https://docs.microsandbox.dev/sdk/overview) for their equivalents.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for building, testing, and the pull-request workflow.

## Community

Questions and discussion happen in the [microsandbox Discord](https://discord.gg/T95Y3XnEAK). For issues specific to the operator, open a [GitHub issue](https://github.com/TheRealSibasishBehera/msb-operator/issues).

## License

Apache-2.0. See [LICENSE](LICENSE).
