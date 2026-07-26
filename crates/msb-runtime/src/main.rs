//! The msb-runtime container: links the msb SDK, boots the sandbox, supervises
//! it until it stops, and exits with its status.
//!
//! It must not detach. Under Kubernetes, when this entrypoint exits containerd
//! tears down the container cgroup and kills the VMM with it, so the runtime
//! process has to live as long as the sandbox does.

mod net;
mod secrets;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use microsandbox::Sandbox;
use microsandbox::config::set_sdk_msb_path;
use microsandbox::sandbox::PullPolicy;
use microsandbox::set_libkrunfw_path;
use msb_crd::{ResolvedSecret, SandboxSpec};
use tracing::info;

#[derive(Parser)]
#[command(name = "msb-runtime")]
struct Cli {
    /// Sandbox spec as JSON (plain, from the controller).
    #[arg(long, env = "MSB_SANDBOX_SPEC")]
    spec: String,

    /// msb's flat sandbox name (encoded namespace/name).
    #[arg(long, env = "MSB_SANDBOX_NAME")]
    sandbox_name: String,

    /// The baked msb binary the SDK spawns.
    #[arg(long, default_value = "/usr/local/bin/msb", env = "MSB_PATH")]
    msb_path: PathBuf,

    #[arg(
        long,
        default_value = "/usr/local/lib/libkrunfw.so",
        env = "MSB_LIBKRUNFW_PATH"
    )]
    libkrunfw_path: PathBuf,

    #[arg(long, default_value = "/msb", env = "MSB_HOME")]
    msb_home: PathBuf,

    /// Root of the kubelet-mounted Secret volumes: `<dir>/<secretName>/<key>`.
    #[arg(long, default_value = "/msb-secrets", env = "MSB_SECRETS_DIR")]
    secrets_dir: PathBuf,
}

/// Reads each referenced Secret's plaintext from its kubelet-mounted volume (no
/// API access) and resolves it. A missing file is a hard error: the pod fails
/// closed rather than booting with an empty secret.
fn resolve_secrets(
    spec: &SandboxSpec,
    secrets_dir: &std::path::Path,
) -> Result<Vec<ResolvedSecret>> {
    let mut plaintext = BTreeMap::new();
    for (name, key) in secrets::required_keys(&spec.secrets) {
        let path = secrets_dir.join(&name).join(&key);
        let value = std::fs::read_to_string(&path)
            .with_context(|| format!("reading secret {name}/{key} at {}", path.display()))?;
        plaintext.insert((name, key), value.trim_end_matches('\n').to_string());
    }
    secrets::resolve(&spec.secrets, &plaintext).context("resolving secrets")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let spec: SandboxSpec = serde_json::from_str(&cli.spec).context("parsing MSB_SANDBOX_SPEC")?;
    let secrets = resolve_secrets(&spec, &cli.secrets_dir)?;

    // The SDK's own setters, not env mutation — the workspace forbids unsafe.
    set_sdk_msb_path(&cli.msb_path);
    set_libkrunfw_path(&cli.libkrunfw_path);

    let mut builder = Sandbox::builder(&cli.sandbox_name)
        // Pass the app image reference, not the VMDK path: with the cache baked
        // under $MSB_HOME/cache and PullPolicy::Never, msb resolves it to the
        // baked overlay with no pull. A VMDK path would become a raw DiskImage
        // and lose msb's layer dedup.
        .image(spec.image.clone())
        .pull_policy(PullPolicy::Never)
        .cpus(spec.cpus as u8)
        .memory(spec.memory);

    if !spec.cmd.is_empty() {
        // Ties the VM's lifetime to the command (it stops when the command
        // exits); `initial_command` would instead leave the VM up for exec.
        builder = builder.persistent_initial_command(spec.cmd.clone());
    }

    // Apply the network policy and secret substitution; without this the guest
    // boots with neither.
    builder = net::apply(builder, &spec, &secrets);

    info!(
        sandbox = %cli.sandbox_name,
        image = %spec.image,
        "booting sandbox from pre-baked cache"
    );
    let config = builder.build().await.context("building sandbox config")?;
    // `create` (not `create_detached`) returns once the guest is ready and
    // leaves this process owning the VMM; `wait` then blocks until it exits.
    let sandbox = Sandbox::create(config)
        .await
        .context("create: booting the sandbox")?;
    info!(sandbox = %cli.sandbox_name, "sandbox booted; supervising until it exits");

    let stop = sandbox
        .wait_until_stopped()
        .await
        .context("wait: supervising the sandbox")?;
    info!(
        sandbox = %cli.sandbox_name,
        exit_code = ?stop.exit_code,
        signal = ?stop.signal,
        "sandbox exited"
    );

    // Exit with the sandbox's status so the container's exit reflects it; the
    // controller reads phase/reason from Pod status. Signal maps to 128+signal.
    let code = match (stop.exit_code, stop.signal) {
        (_, Some(sig)) => 128 + sig,
        (Some(c), None) => c,
        (None, None) => 0,
    };
    std::process::exit(code);
}
