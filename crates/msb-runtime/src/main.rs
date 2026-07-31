//! The msb-runtime container: links the msb SDK, boots the sandbox, supervises
//! it until it stops, and exits with its status.
//!
//! It must not detach. Under Kubernetes, when this entrypoint exits containerd
//! tears down the container cgroup and kills the VMM with it, so the runtime
//! process has to live as long as the sandbox does.

mod cache_wait;
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

/// Parse a Kubernetes resource.Quantity (e.g. `4Gi`, `512Mi`, `2G`) to whole
/// MiB, rounding up. Only the binary/decimal suffixes a disk size sensibly uses.
fn quantity_to_mib(q: &str) -> Result<u32> {
    let q = q.trim();
    let (num, mult_bytes): (&str, u64) = if let Some(n) = q.strip_suffix("Gi") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = q.strip_suffix("Mi") {
        (n, 1024 * 1024)
    } else if let Some(n) = q.strip_suffix("Ki") {
        (n, 1024)
    } else if let Some(n) = q.strip_suffix('G') {
        (n, 1_000_000_000)
    } else if let Some(n) = q.strip_suffix('M') {
        (n, 1_000_000)
    } else {
        (q, 1)
    };
    let value: f64 = num.trim().parse().with_context(|| format!("invalid quantity {q:?}"))?;
    anyhow::ensure!(value >= 0.0, "quantity {q:?} must be non-negative");
    let bytes = value * mult_bytes as f64;
    let mib = (bytes / (1024.0 * 1024.0)).ceil();
    Ok(mib as u32)
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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Log elapsed-since-start at each boundary so the boot path is a grep.
    let t0 = std::time::Instant::now();
    let phase =
        |name: &str| info!(elapsed_ms = t0.elapsed().as_millis() as u64, phase = name, "boot phase");

    let cli = Cli::parse();
    let spec: SandboxSpec = serde_json::from_str(&cli.spec).context("parsing MSB_SANDBOX_SPEC")?;
    phase("parsed spec");

    // Boot is PullPolicy::Never, so the daemon must have populated the cache
    // first. Wait for its ready marker before touching the SDK.
    cache_wait::wait(&cli.msb_home.join("cache"), &spec.image)
        .await
        .context("waiting for the node cache")?;
    phase("cache ready");

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

    // Writable overlay ("upper") capacity for the guest's `/`. Unset in the CRD
    // defaults to msb's own 4 GiB, so this only bites when the user overrides.
    builder = builder.root_disk(quantity_to_mib(&spec.upper.size)?);

    if !spec.entrypoint.is_empty() {
        builder = builder.entrypoint(spec.entrypoint.clone());
    }
    if !spec.cmd.is_empty() {
        // Background (detached) launch: the VM stops when the command exits
        // (run-to-completion). Foreground is the attached, one-shot `msb run` path.
        builder = builder.background_command(spec.cmd.clone());
    }
    for e in &spec.env {
        builder = builder.env(&e.name, &e.value);
    }
    if let Some(w) = &spec.workdir {
        builder = builder.workdir(w);
    }
    if let Some(s) = &spec.shell {
        builder = builder.shell(s);
    }
    if let Some(u) = &spec.user {
        builder = builder.user(u);
    }
    if let Some(h) = &spec.hostname {
        builder = builder.hostname(h);
    }

    // The VM launcher enforces these, so no controller-side deadline is needed.
    if let Some(secs) = spec.max_duration_secs {
        builder = builder.max_duration(secs);
    }
    if let Some(secs) = spec.idle_timeout_secs {
        builder = builder.idle_timeout(secs);
    }

    // Apply the network policy and secret substitution; without this the guest
    // boots with neither.
    builder = net::apply(builder, &spec, &secrets)?;

    info!(
        sandbox = %cli.sandbox_name,
        image = %spec.image,
        "booting sandbox from pre-baked cache"
    );
    let config = builder.build().await.context("building sandbox config")?;
    phase("sdk config built");
    // `create` (not `create_detached`) returns once the guest is ready and
    // leaves this process owning the VMM; `wait` then blocks until it exits.
    let sandbox = Sandbox::create(config)
        .await
        .context("create: booting the sandbox")?;
    phase("guest booted");
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

#[cfg(test)]
mod tests {
    use super::quantity_to_mib;

    #[test]
    fn parses_quantities_to_mib() {
        assert_eq!(quantity_to_mib("4Gi").unwrap(), 4096);
        assert_eq!(quantity_to_mib("8Gi").unwrap(), 8192);
        assert_eq!(quantity_to_mib("512Mi").unwrap(), 512);
        assert_eq!(quantity_to_mib("1Ki").unwrap(), 1); // rounds up
        assert_eq!(quantity_to_mib("1G").unwrap(), 954); // 1e9 bytes -> 953.7 MiB, ceil
        assert_eq!(quantity_to_mib("1024").unwrap(), 1); // bare bytes
        assert!(quantity_to_mib("garbage").is_err());
        assert!(quantity_to_mib("-1Gi").is_err());
    }
}
