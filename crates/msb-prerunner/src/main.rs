mod secrets;

use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use msb_crd::{ResolvedSecret, SandboxSpec};
use tracing::info;

/// Init container: read the plaintext of each referenced Secret from the volume
/// the kubelet mounted, pair it with the spec's metadata, and write the resolved
/// secrets to the shared config volume for the runtime. No Kubernetes API access
/// — the kubelet did the Secret read (see the design doc).
#[derive(Parser)]
#[command(name = "msb-prerunner")]
struct Cli {
    /// Sandbox spec as JSON (set by the controller). Not base64 — carries only
    /// secret references, and stays legible in `kubectl describe`.
    #[arg(long, env = "MSB_SANDBOX_SPEC")]
    spec: String,

    /// Root of the mounted Secret volumes: `<dir>/<secretName>/<key>`.
    #[arg(long, default_value = "/msb-secrets", env = "MSB_SECRETS_DIR")]
    secrets_dir: PathBuf,

    /// Where to write the resolved secrets (on the shared tmpfs).
    #[arg(
        long,
        default_value = "/msb-config/secrets.json",
        env = "MSB_SECRETS_OUT"
    )]
    secrets_out: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let spec: SandboxSpec =
        serde_json::from_str(&cli.spec).context("parsing MSB_SANDBOX_SPEC as SandboxSpec")?;

    let plaintext = read_secret_files(&spec, &cli.secrets_dir)?;
    let resolved = secrets::resolve(&spec.secrets, &plaintext).context("resolving secrets")?;

    write_secrets(&cli.secrets_out, &resolved)?;
    info!(
        count = resolved.len(),
        out = %cli.secrets_out.display(),
        "wrote resolved secrets"
    );

    Ok(())
}

/// Reads each referenced `(secret, key)` plaintext from the mounted Secret file
/// at `<secrets_dir>/<secret>/<key>`. A missing file is a hard error, so the pod
/// fails closed (`Init:Error`) rather than booting with an empty secret.
fn read_secret_files(
    spec: &SandboxSpec,
    secrets_dir: &std::path::Path,
) -> Result<BTreeMap<(String, String), String>> {
    let mut out = BTreeMap::new();
    for (secret_name, key) in secrets::required_keys(&spec.secrets) {
        let path = secrets_dir.join(&secret_name).join(&key);
        let value = std::fs::read_to_string(&path)
            .with_context(|| format!("reading secret {secret_name}/{key} at {}", path.display()))?;
        // Trim a trailing newline so the substituted value is exactly the secret.
        out.insert((secret_name, key), value.trim_end_matches('\n').to_string());
    }
    Ok(out)
}

/// Writes the resolved secrets mode 0600 via a temp file + atomic rename, so the
/// runtime never reads a half-written file. 0600 (despite tmpfs) keeps the
/// plaintext unreadable to any other uid sharing the pod.
fn write_secrets(path: &std::path::Path, resolved: &[ResolvedSecret]) -> Result<()> {
    let json = serde_json::to_vec(resolved).context("serialising resolved secrets")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("opening {}", tmp.display()))?;
        use std::io::Write;
        file.write_all(&json)
            .with_context(|| format!("writing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
