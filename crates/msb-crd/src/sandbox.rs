use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// CEL immutability: all spec fields are sealed at creation time.
// The API server enforces this server-side; the controller never needs to check.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "sandbox.microsandbox.io",
    version = "v1alpha1",
    kind = "Sandbox",
    namespaced,
    status = "SandboxStatus",
    shortname = "sb",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].status"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.terminationReason"}"#,
    printcolumn = r#"{"name":"Exit","type":"integer","jsonPath":".status.exitCode"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".status.nodeName"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[kube(schema = "derived")]
#[serde(rename_all = "camelCase")]
pub struct SandboxSpec {
    /// OCI image for the guest root filesystem.
    pub image: String,

    /// Number of vCPUs.
    #[serde(default = "default_cpus")]
    pub cpus: u32,

    /// Memory in MiB.
    #[serde(default = "default_memory_mib")]
    pub memory: u32,

    /// Command to run inside the guest. Defaults to the image entrypoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,

    /// Whether to delete the Sandbox CRD after the sandbox exits.
    #[serde(default)]
    pub ephemeral: bool,

    /// Restart policy on failure.
    #[serde(default)]
    pub run_policy: RunPolicy,

    /// Secrets injected as env vars inside the guest via the smoltcp proxy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretEntry>,

    /// Network configuration.
    #[serde(default)]
    pub network: NetworkSpec,

    /// Writable overlay layer configuration.
    #[serde(default)]
    pub upper: UpperSpec,

    /// Named volumes (node-local in V1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeSpec>,
}

fn default_cpus() -> u32 {
    1
}

fn default_memory_mib() -> u32 {
    512
}

// --- Secrets ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretEntry {
    /// Environment variable name the guest sees (set to the placeholder value).
    pub env: String,

    /// Source of the secret value.
    pub value_from: SecretValueFrom,

    /// Hosts the proxy is permitted to substitute this value to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretValueFrom {
    pub secret_key_ref: SecretKeyRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    pub name: String,
    pub key: String,
}

/// A secret the prerunner has resolved to plaintext, handed to the runtime over
/// the shared config volume. Not part of the CRD — the on-disk contract between
/// the two containers. `value` is plaintext and must stay on the tmpfs mount.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSecret {
    pub env: String,
    pub value: String,
    pub placeholder: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

// Redact `value` — a resolved secret must never reach a log line.
impl std::fmt::Debug for ResolvedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSecret")
            .field("env", &self.env)
            .field("value", &"<redacted>")
            .field("placeholder", &self.placeholder)
            .field("allowed_hosts", &self.allowed_hosts)
            .finish()
    }
}

// --- Network ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSpec {
    /// Disable the smoltcp proxy entirely.
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default)]
    pub policy: NetworkPolicySpec,

    #[serde(default)]
    pub tls: TlsSpec,

    #[serde(default)]
    pub dns: DnsSpec,

    /// Guest ports to expose on the Pod IP.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published_ports: Vec<PublishedPort>,

    /// Maximum concurrent guest TCP connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// Copy host trusted CAs into the guest.
    #[serde(default)]
    pub trust_host_cas: bool,
}

impl Default for NetworkSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            policy: NetworkPolicySpec::default(),
            tls: TlsSpec::default(),
            dns: DnsSpec::default(),
            published_ports: Vec::new(),
            max_connections: default_max_connections(),
            trust_host_cas: false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_max_connections() -> u32 {
    256
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicySpec {
    #[serde(default)]
    pub preset: PolicyPreset,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PolicyPreset {
    #[default]
    PublicOnly,
    AllowAll,
    DenyAll,
    NonLocal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TlsSpec {
    #[serde(default)]
    pub intercept: bool,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intercepted_ports: Vec<InterceptedPort>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InterceptedPort {
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublishedPort {
    pub container_port: u16,

    #[serde(default)]
    pub protocol: PortProtocol,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PortProtocol {
    #[default]
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DnsSpec {
    #[serde(default = "default_true")]
    pub rebind_protection: bool,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nameservers: Vec<String>,

    #[serde(default = "default_dns_timeout_ms")]
    pub query_timeout_ms: u32,
}

fn default_dns_timeout_ms() -> u32 {
    5000
}

impl Default for DnsSpec {
    fn default() -> Self {
        Self {
            rebind_protection: true,
            nameservers: Vec::new(),
            query_timeout_ms: default_dns_timeout_ms(),
        }
    }
}

// --- Storage ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpperSpec {
    /// Writable overlay size as a Kubernetes resource.Quantity string (e.g. "4Gi").
    #[serde(default = "default_upper_size")]
    pub size: String,
}

fn default_upper_size() -> String {
    "4Gi".to_string()
}

impl Default for UpperSpec {
    fn default() -> Self {
        Self {
            size: default_upper_size(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VolumeSpec {
    pub name: String,
    pub mount_path: String,
    /// Volume size as a Kubernetes resource.Quantity string (e.g. "10Gi").
    pub size: String,
}

// --- Run policy ---

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum RunPolicy {
    /// Run once; no retry on any exit.
    #[default]
    Once,
    /// Retry indefinitely on unclean exit; stop on clean exit.
    RerunOnFailure,
}

// --- Status ---

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    pub phase: Option<SandboxPhase>,
    pub pod_name: Option<String>,
    pub node_name: Option<String>,
    pub started_at: Option<String>,
    pub terminated_at: Option<String>,
    pub termination_reason: Option<TerminationReason>,
    pub exit_code: Option<i32>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<SandboxCondition>,
}

/// A status condition, mirroring `metav1.Condition`. schemars-derived because
/// `k8s-openapi`'s `Condition` has no `JsonSchema` impl for the CRD schema.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SandboxCondition {
    /// e.g. `Ready`, `Failed`.
    #[serde(rename = "type")]
    pub type_: String,
    /// `True`, `False`, or `Unknown`.
    pub status: String,
    /// PascalCase machine-readable reason.
    pub reason: String,
    pub message: String,
    pub last_transition_time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum SandboxPhase {
    Pending,
    Running,
    Succeeded,
    Failed,
}

/// Termination reason sourced from the daemon annotation or inferred by the controller.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum TerminationReason {
    // Clean exits (from daemon annotation)
    Completed,
    MaxDurationExceeded,
    IdleTimeout,
    ShutdownRequested,
    // Unclean exits (from daemon annotation)
    Failed,
    // Operator-inferred (controller reads Pod/Node state; msb never sees these)
    #[serde(rename = "OOMKilled")]
    OomKilled,
    Evicted,
    NodeLost,
}

impl TerminationReason {
    pub fn is_clean(&self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::MaxDurationExceeded
                | Self::IdleTimeout
                | Self::ShutdownRequested
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_termination_reasons() {
        assert!(TerminationReason::Completed.is_clean());
        assert!(TerminationReason::MaxDurationExceeded.is_clean());
        assert!(TerminationReason::IdleTimeout.is_clean());
        assert!(TerminationReason::ShutdownRequested.is_clean());
    }

    #[test]
    fn unclean_termination_reasons() {
        assert!(!TerminationReason::Failed.is_clean());
        assert!(!TerminationReason::OomKilled.is_clean());
        assert!(!TerminationReason::Evicted.is_clean());
        assert!(!TerminationReason::NodeLost.is_clean());
    }

    #[test]
    fn default_spec_values() {
        let spec = SandboxSpec {
            image: "python:3.12".to_string(),
            cpus: default_cpus(),
            memory: default_memory_mib(),
            cmd: vec![],
            ephemeral: false,
            run_policy: RunPolicy::Once,
            secrets: vec![],
            network: NetworkSpec::default(),
            upper: UpperSpec::default(),
            volumes: vec![],
        };
        assert_eq!(spec.cpus, 1);
        assert_eq!(spec.memory, 512);
        assert_eq!(spec.network.max_connections, 256);
        assert!(spec.network.enabled);
        assert_eq!(spec.upper.size, "4Gi");
    }
}
