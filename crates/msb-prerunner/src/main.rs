mod launch;
mod secrets;

use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use msb_crd::SandboxSpec;
use tracing::info;

/// Init container: resolve `secretKeyRef`s and write the msb `LaunchConfig` to
/// the shared config volume. (Binary sideload lives in the image entrypoint.)
#[derive(Parser)]
#[command(name = "msb-prerunner")]
struct Cli {
    /// Sandbox spec as JSON (set by the controller). Not base64 — carries only
    /// secret references, and stays legible in `kubectl describe`.
    #[arg(long, env = "MSB_SANDBOX_SPEC")]
    spec: String,

    /// msb's flat sandbox name (already encoded from namespace/name).
    #[arg(long, env = "MSB_SANDBOX_NAME")]
    sandbox_name: String,

    #[arg(long, env = "MSB_NAMESPACE")]
    namespace: String,

    #[arg(long, default_value = "/msb", env = "MSB_HOME")]
    msb_home: PathBuf,

    /// Baked cache VMDK the sandbox boots from.
    #[arg(long, env = "MSB_ROOTFS_VMDK")]
    rootfs_vmdk: PathBuf,

    #[arg(
        long,
        default_value = "/msb-bin/libkrunfw.so",
        env = "MSB_LIBKRUNFW_PATH"
    )]
    libkrunfw_path: PathBuf,

    #[arg(long, default_value = "/msb-config/sandbox.json")]
    config_out: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let spec: SandboxSpec =
        serde_json::from_str(&cli.spec).context("parsing MSB_SANDBOX_SPEC as SandboxSpec")?;

    let resolved = resolve_secrets(&spec, &cli.namespace).await?;
    let secret_entries =
        secrets::resolve(&spec.secrets, &resolved).context("building resolved secret entries")?;

    let config = launch::build(
        &spec,
        &cli.msb_home,
        &cli.sandbox_name,
        cli.rootfs_vmdk,
        cli.libkrunfw_path,
        secret_entries,
    );

    write_config(&cli.config_out, &config)?;
    info!(sandbox = %cli.sandbox_name, out = %cli.config_out.display(), "wrote LaunchConfig");

    Ok(())
}

/// Fetches every referenced Secret key from the API. A missing Secret or key is
/// a hard error: the pod enters `Init:Error` and `msb-runtime` never starts,
/// which is the intended fail-closed behaviour for a missing secret.
async fn resolve_secrets(
    spec: &SandboxSpec,
    namespace: &str,
) -> Result<BTreeMap<(String, String), String>> {
    let needed = secrets::required_keys(&spec.secrets);
    if needed.is_empty() {
        return Ok(BTreeMap::new());
    }

    let client = Client::try_default()
        .await
        .context("connecting to the Kubernetes API")?;
    let api: Api<Secret> = Api::namespaced(client, namespace);

    // Cache each Secret object so multiple keys from one Secret cost one GET.
    let mut objects: BTreeMap<String, Secret> = BTreeMap::new();
    let mut out = BTreeMap::new();

    for (secret_name, key) in needed {
        let secret = match objects.get(&secret_name) {
            Some(s) => s,
            None => {
                let fetched = api
                    .get(&secret_name)
                    .await
                    .with_context(|| format!("fetching Secret {namespace}/{secret_name}"))?;
                objects.entry(secret_name.clone()).or_insert(fetched)
            }
        };

        let value = secret_value(secret, &key)
            .with_context(|| format!("reading key {key} from Secret {namespace}/{secret_name}"))?;
        out.insert((secret_name, key), value);
    }

    Ok(out)
}

/// Reads a key from a Secret's `data` (base64-decoded by kube) or `stringData`.
fn secret_value(secret: &Secret, key: &str) -> Result<String> {
    if let Some(byte_string) = secret.data.as_ref().and_then(|d| d.get(key)) {
        return String::from_utf8(byte_string.0.clone())
            .with_context(|| format!("Secret key {key} is not valid UTF-8"));
    }
    if let Some(value) = secret.string_data.as_ref().and_then(|d| d.get(key)) {
        return Ok(value.clone());
    }
    bail!("key {key} not present in Secret")
}

/// Writes the config mode 0600 via a temp file + atomic rename, so `msb-runtime`
/// never sees a half-written config. 0600 (despite tmpfs) keeps the plaintext
/// unreadable to any other uid sharing the pod.
fn write_config(path: &std::path::Path, config: &launch::LaunchConfig) -> Result<()> {
    let json = serde_json::to_vec(config).context("serialising LaunchConfig")?;
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
