//! Drives the gateway with the unmodified upstream microsandbox SDK. Passing
//! proves the contract: an existing SDK works against the cluster with only
//! MSB_API_URL + MSB_API_KEY set. Exit code is the result.

use microsandbox::{BackendKind, CloudBackend, MicrosandboxError, Sandbox, default_backend, set_default_backend};

const IMAGE: &str = "alpine:3.20";
const MARKER: &str = "hello-from-guest";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    set_default_backend(CloudBackend::from_env()?);
    anyhow::ensure!(
        default_backend().kind() == BackendKind::Cloud,
        "expected the cloud backend — check MSB_API_URL/MSB_API_KEY"
    );

    let name = "sdk-gateway-e2e";

    // .replace() is Unsupported on cloud; pre-clean with remove so a leftover
    // from a failed run doesn't collide with create.
    let _ = Sandbox::remove(name).await;

    let sandbox = Sandbox::builder(name)
        .image(IMAGE)
        .cpus(1)
        .memory(512u32)
        .create()
        .await?;
    println!("PASS: created");

    // uname -m is the guest arch — proves exec ran IN the microVM, not a gateway echo.
    let out = sandbox
        .exec("sh", ["-c".to_string(), format!("echo {MARKER}; uname -m")])
        .await?;
    let stdout = out.stdout()?;
    anyhow::ensure!(out.status().success, "exec failed: {:?}", out.status());
    anyhow::ensure!(stdout.contains(MARKER), "missing marker: {stdout:?}");
    anyhow::ensure!(stdout.contains("x86_64"), "not the guest arch: {stdout:?}");
    println!("PASS: exec ran in guest");

    match sandbox.stop().await {
        Ok(()) => {}
        Err(MicrosandboxError::SandboxNotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
    let _ = Sandbox::remove(name).await;
    println!("PASS: stopped + removed");

    println!("== SDK-gateway e2e PASSED ==");
    Ok(())
}
