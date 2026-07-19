//! Hand-mirrored subset of microsandbox's `LaunchConfig`, serialized to the JSON
//! `msb sandbox` reads on its config fd.
//!
//! We mirror rather than depend on the msb crate: msb pins its internal crates
//! at exact versions and promises no API stability across patch releases. The
//! cost is that nothing in the type system catches schema drift, so two guards
//! stand in: the always-on shape tests below (all keys present, `Option`s as
//! explicit `null`, exact enum strings), and a feature-gated round-trip test
//! that deserializes our JSON into the real `microsandbox-runtime::LaunchConfig`
//! where that crate is available (CI).
//!
//! Field names and serde attributes must stay byte-exact with the msb source.
//! `LaunchConfig` has no field defaults, so every key must be emitted. The
//! `network` subtree is `#[serde(default)]` throughout, so we carry only the
//! fields the `Sandbox` CRD exposes and let msb default the rest.

mod network;

pub use network::{HostPattern, NetworkConfig, SecretEntry, network_from_spec};

use std::path::{Path, PathBuf};

use msb_crd::SandboxSpec;
use serde::Serialize;
use sha2::{Digest, Sha256};

const DB_CONNECT_TIMEOUT_SECS: u64 = 10;

/// One sandbox per pod, so the slot is always 0 (no per-node IPAM).
const SANDBOX_SLOT: u64 = 0;

/// Hex of the first 16 bytes of `sha256(name)` — msb's agent-socket basename.
fn agent_socket_hash(name: &str) -> String {
    let digest = Sha256::digest(name.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Builds the `LaunchConfig`. Pure: `secrets` arrive already resolved and
/// `rootfs_vmdk` already materialized, so the caller does all I/O.
pub fn build(
    spec: &SandboxSpec,
    msb_home: &Path,
    sandbox_name: &str,
    rootfs_vmdk: PathBuf,
    libkrunfw_path: PathBuf,
    secrets: Vec<SecretEntry>,
) -> LaunchConfig {
    let sandboxes_dir = msb_home.join("sandboxes");
    let sandbox_dir = sandboxes_dir.join(sandbox_name);

    let agent_sock = msb_home
        .join("run")
        .join("agent")
        .join(format!("{}.sock", agent_socket_hash(sandbox_name)));

    let (exec_path, exec_args) = match spec.cmd.split_first() {
        Some((head, tail)) => (Some(PathBuf::from(head)), tail.to_vec()),
        None => (None, Vec::new()),
    };

    LaunchConfig {
        db_path: msb_home.join("db").join("msb.db"),
        db_connect_timeout_secs: DB_CONNECT_TIMEOUT_SECS,
        log_dir: sandbox_dir.join("logs"),
        runtime_dir: sandbox_dir.join("runtime"),
        sandboxes_dir,
        agent_sock,
        libkrunfw_path,
        startup: None,
        lifecycle: Lifecycle::default(),
        metrics: MetricsConfig::default(),
        // Read-only VMDK (fsmeta + layer EROFS) + writable upper.ext4, matching
        // the SDK's OCI boot layout (spawn.rs:2086-2088).
        rootfs: RootfsConfig {
            disk: Some(rootfs_vmdk),
            disk_format: Some("vmdk".to_string()),
            upper: Some(sandbox_dir.join("upper.ext4")),
            ..Default::default()
        },
        mounts: Vec::new(),
        disks: Vec::new(),
        init_path: None,
        // msb reads guest env from `KEY=VALUE` strings; the image/cmd provide the
        // rest. Secret placeholders are injected by the proxy, not set here.
        env: Vec::new(),
        workdir: None,
        exec_path,
        exec_args,
        network: Some(network_from_spec(spec, secrets)),
        sandbox_slot: SANDBOX_SLOT,
    }
}

/// No `#[serde(default)]` on msb's side, so every key is mandatory on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct LaunchConfig {
    pub db_path: PathBuf,
    pub db_connect_timeout_secs: u64,
    pub log_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub sandboxes_dir: PathBuf,
    pub agent_sock: PathBuf,
    pub libkrunfw_path: PathBuf,
    pub startup: Option<StartupCommand>,
    pub lifecycle: Lifecycle,
    pub metrics: MetricsConfig,
    pub rootfs: RootfsConfig,
    /// `tag:host_path[:opts]`
    pub mounts: Vec<String>,
    /// `id:host_path:format[:ro]`
    pub disks: Vec<String>,
    pub init_path: Option<PathBuf>,
    /// `KEY=VALUE`
    pub env: Vec<String>,
    pub workdir: Option<PathBuf>,
    pub exec_path: Option<PathBuf>,
    pub exec_args: Vec<String>,
    // `network` and `sandbox_slot` are `#[cfg(feature = "net")]` in msb, and the
    // shipped binary builds with `net` on, so both keys are expected.
    pub network: Option<NetworkConfig>,
    pub sandbox_slot: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Lifecycle {
    pub max_duration_secs: Option<u64>,
    pub idle_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MetricsConfig {
    pub sample_interval_ms: u64,
    pub disabled: bool,
    pub slot: Option<MetricsSlotHandoff>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSlotHandoff {
    pub shm_name: String,
    pub slot: u32,
    pub generation: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RootfsConfig {
    pub path: Option<PathBuf>,
    pub disk: Option<PathBuf>,
    pub disk_format: Option<String>,
    pub disk_readonly: bool,
    pub upper: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartupCommand {
    pub cmd: String,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub cwd: Option<String>,
    pub user: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use msb_crd::SandboxSpec;
    use serde_json::Value;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            image: "registry.example.com/py:latest".to_string(),
            cpus: 2,
            memory: 1024,
            cmd: vec!["python".to_string(), "app.py".to_string()],
            ephemeral: true,
            run_policy: Default::default(),
            secrets: Vec::new(),
            network: Default::default(),
            upper: Default::default(),
            volumes: Vec::new(),
        }
    }

    fn built() -> Value {
        let cfg = build(
            &spec(),
            Path::new("/msb"),
            "team-a__hello",
            PathBuf::from("/msb/cache/vmdk/img.vmdk"),
            PathBuf::from("/msb-bin/libkrunfw.so"),
            Vec::new(),
        );
        serde_json::to_value(&cfg).expect("serializes")
    }

    /// The structural drift guard: a missing key fails msb's deserialize.
    #[test]
    fn emits_all_twenty_top_level_keys() {
        let v = built();
        let obj = v.as_object().expect("object");
        let expected = [
            "db_path",
            "db_connect_timeout_secs",
            "log_dir",
            "runtime_dir",
            "sandboxes_dir",
            "agent_sock",
            "libkrunfw_path",
            "startup",
            "lifecycle",
            "metrics",
            "rootfs",
            "mounts",
            "disks",
            "init_path",
            "env",
            "workdir",
            "exec_path",
            "exec_args",
            "network",
            "sandbox_slot",
        ];
        for key in expected {
            assert!(obj.contains_key(key), "missing top-level key {key}");
        }
        assert_eq!(obj.len(), expected.len(), "unexpected extra top-level keys");
    }

    /// `None` must be explicit `null`, not an omitted key — msb has no defaults.
    #[test]
    fn none_options_serialize_as_explicit_null() {
        let v = built();
        assert_eq!(v["startup"], Value::Null);
        assert_eq!(v["init_path"], Value::Null);
        assert_eq!(v["workdir"], Value::Null);
    }

    #[test]
    fn cmd_splits_into_exec_path_and_args() {
        let v = built();
        assert_eq!(v["exec_path"], "python");
        assert_eq!(v["exec_args"], serde_json::json!(["app.py"]));
    }

    #[test]
    fn rootfs_points_at_vmdk_with_upper() {
        let v = built();
        assert_eq!(v["rootfs"]["disk"], "/msb/cache/vmdk/img.vmdk");
        assert_eq!(v["rootfs"]["disk_format"], "vmdk");
        assert_eq!(
            v["rootfs"]["upper"],
            "/msb/sandboxes/team-a__hello/upper.ext4"
        );
    }

    #[test]
    fn paths_derive_from_msb_home() {
        let v = built();
        assert_eq!(v["db_path"], "/msb/db/msb.db");
        assert_eq!(v["sandboxes_dir"], "/msb/sandboxes");
    }

    #[test]
    fn agent_socket_is_the_hashed_path() {
        let v = built();
        let sock = v["agent_sock"].as_str().unwrap();
        assert!(sock.starts_with("/msb/run/agent/"), "{sock}");
        assert!(sock.ends_with(".sock"), "{sock}");
        let hash = agent_socket_hash("team-a__hello");
        assert_eq!(hash.len(), 32, "16 bytes hex");
        assert_eq!(sock, format!("/msb/run/agent/{hash}.sock"));
    }

    #[test]
    fn public_only_preset_mirrors_msb_semantics() {
        let v = built();
        let policy = &v["network"]["policy"];
        // default_ingress: allow is load-bearing — Deny would break published ports.
        assert_eq!(policy["default_egress"], "deny");
        assert_eq!(policy["default_ingress"], "allow");

        let rules = policy["rules"].as_array().unwrap();
        // DNS via the gateway (Host group), not Any — the security-relevant bit.
        assert_eq!(
            rules[0]["destination"],
            serde_json::json!({ "group": "host" })
        );
        assert_eq!(rules[0]["protocols"], serde_json::json!(["udp", "tcp"]));
        assert_eq!(
            rules[0]["ports"],
            serde_json::json!([{ "start": 53, "end": 53 }])
        );
        assert_eq!(rules[0]["action"], "allow");
        assert_eq!(
            rules[1]["destination"],
            serde_json::json!({ "group": "public" })
        );
        assert_eq!(rules.len(), 2);
    }

    /// non_local is default-DENY egress with explicit allows (DNS + public +
    /// private). It must NOT be a default-allow deny-list, or link-local/metadata
    /// (169.254.169.254) would fall through — an SSRF hole.
    #[test]
    fn non_local_preset_is_default_deny_not_deny_list() {
        let mut s = spec();
        s.network.policy.preset = msb_crd::sandbox::PolicyPreset::NonLocal;
        let cfg = build(
            &s,
            Path::new("/msb"),
            "n",
            PathBuf::from("/v.vmdk"),
            PathBuf::from("/l.so"),
            Vec::new(),
        );
        let v = serde_json::to_value(&cfg).unwrap();
        let policy = &v["network"]["policy"];
        assert_eq!(policy["default_egress"], "deny", "must be fail-closed");
        assert_eq!(policy["default_ingress"], "allow");
        let groups: Vec<_> = policy["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["destination"]["group"].as_str())
            .collect();
        assert_eq!(groups, vec!["host", "public", "private"]);
    }

    #[test]
    fn published_ports_bind_all_interfaces() {
        let mut s = spec();
        s.network.published_ports = vec![msb_crd::sandbox::PublishedPort {
            container_port: 8080,
            protocol: msb_crd::sandbox::PortProtocol::Tcp,
        }];
        let cfg = build(
            &s,
            Path::new("/msb"),
            "n",
            PathBuf::from("/v.vmdk"),
            PathBuf::from("/l.so"),
            Vec::new(),
        );
        let v = serde_json::to_value(&cfg).unwrap();
        let port = &v["network"]["ports"][0];
        assert_eq!(port["host_port"], 8080);
        assert_eq!(port["guest_port"], 8080);
        assert_eq!(port["protocol"], "tcp");
        assert_eq!(port["host_bind"], "0.0.0.0");
    }
}
