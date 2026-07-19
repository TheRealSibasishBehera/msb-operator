mod secrets;

use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use msb_crd::{ResolvedSecret, SandboxSpec};
use tracing::info;

/// Init container: resolve `secretKeyRef`s from the API and write the resolved
/// secrets to the shared config volume for the runtime to load. The runtime
/// builds the boot config itself via the SDK; the prerunner only handles the
/// part that needs Secret-read access.
#[derive(Parser)]
#[command(name = "msb-prerunner")]
struct Cli {
    /// Sandbox spec as JSON (set by the controller). Not base64 — carries only
    /// secret references, and stays legible in `kubectl describe`.
    #[arg(long, env = "MSB_SANDBOX_SPEC")]
    spec: String,

    /// Namespace to resolve `secretKeyRef`s in.
    #[arg(long, env = "MSB_NAMESPACE")]
    namespace: String,

    /// Where to write the resolved secrets (on the shared tmpfs).
    #[arg(long, default_value = "/msb-config/secrets.json", env = "MSB_SECRETS_OUT")]
    secrets_out: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let spec: SandboxSpec =
        serde_json::from_str(&cli.spec).context("parsing MSB_SANDBOX_SPEC as SandboxSpec")?;

    let fetched = fetch_secrets(&spec, &cli.namespace).await?;
    let resolved = secrets::resolve(&spec.secrets, &fetched).context("resolving secrets")?;

    write_secrets(&cli.secrets_out, &resolved)?;
    info!(
        count = resolved.len(),
        out = %cli.secrets_out.display(),
        "wrote resolved secrets"
    );

    Ok(())
}

/// Fetches every referenced Secret key from the API. A missing Secret or key is
/// a hard error: the pod enters `Init:Error` and the runtime never starts, which
/// is the intended fail-closed behaviour for a missing secret.
async fn fetch_secrets(
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
