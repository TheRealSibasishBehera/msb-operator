use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// Per-field CEL immutability: every field except `desiredState` is sealed at
// creation. The API server enforces it server-side; the controller never checks.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "sandbox.microsandbox.dev",
    version = "v1alpha1",
    kind = "Sandbox",
    namespaced,
    status = "SandboxStatus",
    shortname = "sb",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].status"}"#,
    printcolumn = r#"{"name":"Restarts","type":"integer","jsonPath":".status.restartCount"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    // -o wide only: single-sandbox diagnostics, not list-scanning columns.
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.terminationReason","priority":1}"#,
    printcolumn = r#"{"name":"Exit","type":"integer","jsonPath":".status.exitCode","priority":1}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".status.nodeName","priority":1}"#
)]
#[kube(schema = "derived")]
#[serde(rename_all = "camelCase")]
pub struct SandboxSpec {
    /// OCI image for the guest root filesystem. This field is immutable.
    pub image: String,

    /// Desired lifecycle state. `Stopped` removes the pod but keeps the Sandbox,
    /// restartable by setting `Running`. On the current storage model a restart is
    /// a fresh boot (guest state is not preserved). Mutable.
    #[serde(default)]
    pub desired_state: DesiredState,

    /// Number of vCPUs. Mutable, up to maxCpus: raising it live-resizes a running
    /// sandbox over the control socket when there is boot headroom, otherwise the
    /// sandbox needs a restart to apply it.
    #[serde(default = "default_cpus")]
    pub cpus: u32,

    /// Memory in MiB. Mutable, up to maxMemory: raising it live-resizes a running
    /// sandbox over the control socket when there is boot headroom, otherwise the
    /// sandbox needs a restart to apply it.
    #[serde(default = "default_memory_mib")]
    pub memory: u32,

    /// Boot-time vCPU ceiling. Unset defaults to `cpus` (no live-resize headroom).
    /// This field is immutable: the VMM reserves vCPU slots for this count at
    /// boot, so raising it after boot requires a new pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cpus: Option<u32>,

    /// Boot-time memory ceiling in MiB. Unset defaults to `memory` (no
    /// live-resize headroom). This field is immutable: the VMM reserves the
    /// virtio-mem hotplug region for this size at boot, so raising it after boot
    /// requires a new pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory: Option<u32>,

    /// Command to run inside the guest. Defaults to the image entrypoint. This field is immutable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,

    /// Override the image entrypoint. Empty keeps the image's own entrypoint. This field is immutable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,

    /// Plain (non-secret) environment variables set in the guest. This field is immutable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,

    /// Working directory for guest commands. This field is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,

    /// Default shell for guest sessions. This field is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,

    /// User the guest workload runs as (name, uid, or `uid:gid`). This field is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Guest hostname. This field is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,

    /// Wall-clock expiry deadline and policy. Distinct from
    /// `maxDurationSecs`/`idleTimeoutSecs`, which bound the guest's own runtime.
    #[serde(default)]
    pub lifecycle: Lifecycle,

    /// Hard cap on total sandbox lifetime, in seconds. This field is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_secs: Option<u64>,

    /// Stop the sandbox after this many seconds with no activity. This field is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,

    /// Restart policy on failure. This field is immutable.
    #[serde(default)]
    pub run_policy: RunPolicy,

    /// Secrets injected as env vars inside the guest via the smoltcp proxy. This field is immutable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretEntry>,

    /// Network configuration. This field is immutable.
    #[serde(default)]
    pub network: NetworkSpec,

    /// Writable overlay layer configuration. This field is immutable.
    #[serde(default)]
    pub upper: UpperSpec,

    /// In-guest hardening applied to exec sessions. This field is immutable.
    #[serde(default)]
    pub security_profile: SecurityProfile,

    /// POSIX resource limits applied to guest processes at agentd startup. This field is immutable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rlimits: Vec<Rlimit>,

    /// Guest console log capture, opt-in. This field is immutable.
    #[serde(default)]
    pub logging: LoggingSpec,
}

fn default_cpus() -> u32 {
    1
}

fn default_memory_mib() -> u32 {
    512
}

impl SandboxSpec {
    /// The boot-time vCPU ceiling: `maxCpus` if set, else `cpus`.
    pub fn effective_max_cpus(&self) -> u32 {
        self.max_cpus.unwrap_or(self.cpus).max(self.cpus)
    }

    /// The boot-time memory ceiling in MiB: `maxMemory` if set, else `memory`.
    pub fn effective_max_memory(&self) -> u32 {
        self.max_memory.unwrap_or(self.memory).max(self.memory)
    }
}

/// A plain (non-secret) environment variable set in the guest.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    pub name: String,
    pub value: String,
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

/// A secret resolved to plaintext for the runtime. Not part of the CRD — the
/// runtime reads the kubelet-mounted Secret volumes and builds these in memory.
/// `value` is plaintext and is never written anywhere.
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
    /// Port the guest listens on.
    pub guest_port: u16,

    /// Port exposed on the pod. Defaults to `guest_port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,

    #[serde(default)]
    pub protocol: PortProtocol,

    /// Pod-side bind address. Defaults to `0.0.0.0` so the port is reachable on
    /// the Pod IP (msb's own default is loopback, which is unreachable in a pod).
    #[serde(default = "default_host_bind")]
    pub host_bind: String,
}

fn default_host_bind() -> String {
    "0.0.0.0".to_string()
}

impl PublishedPort {
    /// The pod-side port, defaulting to the guest port.
    pub fn host_port(&self) -> u16 {
        self.host_port.unwrap_or(self.guest_port)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    /// Writable overlay size for the guest root, as a Kubernetes
    /// `resource.Quantity` (e.g. `4Gi`, `512Mi`).
    #[serde(default = "default_upper_size")]
    pub size: Quantity,
}

fn default_upper_size() -> Quantity {
    Quantity("4Gi".to_string())
}

impl Default for UpperSpec {
    fn default() -> Self {
        Self {
            size: default_upper_size(),
        }
    }
}

// --- Security & limits ---

/// In-guest hardening for exec sessions. `Restricted` makes agentd set
/// `no_new_privs`, drop `CAP_SYS_ADMIN`, and force `nosuid,nodev` on user mounts
/// (breaks in-guest `sudo`/DinD/mount-admin). Applied inside the VM by the guest
/// kernel, so it needs no host privilege.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum SecurityProfile {
    #[default]
    Default,
    Restricted,
}

/// A POSIX resource limit applied to guest processes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Rlimit {
    pub resource: RlimitResource,
    /// Soft limit; the process may raise it up to `hard`.
    pub soft: u64,
    /// Hard ceiling.
    pub hard: u64,
}

/// The POSIX resource an `Rlimit` bounds (the `RLIMIT_*` family).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum RlimitResource {
    Cpu,
    Fsize,
    Data,
    Stack,
    Core,
    Rss,
    Nproc,
    Nofile,
    Memlock,
    As,
    Locks,
    Sigpending,
    Msgqueue,
    Nice,
    Rtprio,
    Rttime,
}

// --- Logging ---

/// Guest console log capture, opt-in. When `guest_console` is true the pod
/// carries an extra `msb-console-log` container that tails the guest's
/// stdout/stderr, separate from runtime infra logs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoggingSpec {
    #[serde(default)]
    pub guest_console: bool,
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
    /// Name of the per-sandbox ClusterIP Service. Hashed, so not guessable —
    /// clients read it here to reach the sandbox's bridge.
    pub service_name: Option<String>,
    pub node_name: Option<String>,
    /// When the guest reached Running, as a Kubernetes `metav1.Time` (RFC3339).
    pub started_at: Option<Time>,
    /// When the guest terminated, as a Kubernetes `metav1.Time` (RFC3339).
    pub terminated_at: Option<Time>,
    pub termination_reason: Option<TerminationReason>,
    pub exit_code: Option<i32>,

    /// Times the pod has been recreated under `runPolicy: RerunOnFailure`. Drives
    /// the requeue backoff and is surfaced so users can see a sandbox is looping.
    #[serde(default)]
    pub restart_count: u32,

    /// Ports exposed on the per-sandbox Service, reflected so `kubectl describe`
    /// shows where a guest listener is reachable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exposed_ports: Vec<ExposedPort>,

    /// The `cpus`/`memory` values last confirmed applied to the running guest —
    /// either at boot or via a live resize. Diverges from `spec.cpus`/`spec.memory`
    /// when an edit is pending a live resize or is `RestartRequired`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_cpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_memory: Option<u32>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// A port reachable on the per-sandbox Service.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExposedPort {
    /// Port on the Service (and Pod).
    pub port: u16,
    pub protocol: PortProtocol,
    /// The Service port name, referenceable by Ingress/Istio.
    pub name: String,
}

/// High-level lifecycle phase the controller writes to `.status.phase`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum SandboxPhase {
    Pending,
    Running,
    /// At rest by `desiredState: Stopped` — no pod, but restartable. Distinct from
    /// `Succeeded`/`Failed`, which are the guest exiting on its own.
    Stopped,
    Succeeded,
    Failed,
}

/// Desired lifecycle state the user sets on `spec.desiredState`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum DesiredState {
    #[default]
    Running,
    Stopped,
}

/// Expiry policy: an absolute deadline plus what to do with the object once it
/// passes. A guest exiting on its own never triggers this; only the deadline does.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Lifecycle {
    /// Absolute time the sandbox expires. Unset means it never expires on a
    /// schedule. Mutable: adjust or clear it to extend, shorten, or cancel expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_time: Option<Time>,

    /// What happens to the Sandbox object at expiry. Its pod and Service are
    /// always deleted; this governs the object itself. Mutable.
    #[serde(default)]
    pub shutdown_policy: ShutdownPolicy,
}

/// What to do with the Sandbox object when it expires.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum ShutdownPolicy {
    /// Keep the object with an `Expired` status.
    #[default]
    Retain,
    /// Delete the object.
    Delete,
}

/// Why the sandbox ended, derived by the controller from the runtime container's
/// terminated state.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum TerminationReason {
    // Clean exits
    Completed,
    MaxDurationExceeded,
    IdleTimeout,
    ShutdownRequested,
    // Unclean exit
    Failed,
    // Operator-inferred (controller reads Pod/Node state; msb never sees these)
    #[serde(rename = "OOMKilled")]
    OomKilled,
    Evicted,
    NodeLost,
    /// The `spec.lifecycle.shutdownTime` deadline passed.
    Expired,
}

impl TerminationReason {
    pub fn is_clean(&self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::MaxDurationExceeded
                | Self::IdleTimeout
                | Self::ShutdownRequested
                | Self::Expired
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
            max_cpus: None,
            max_memory: None,
            cmd: vec![],
            entrypoint: vec![],
            env: vec![],
            workdir: None,
            shell: None,
            user: None,
            hostname: None,
            lifecycle: Default::default(),
            max_duration_secs: None,
            idle_timeout_secs: None,
            run_policy: RunPolicy::Once,
            secrets: vec![],
            network: NetworkSpec::default(),
            upper: UpperSpec::default(),
            security_profile: SecurityProfile::default(),
            rlimits: vec![],
            logging: Default::default(),
            desired_state: Default::default(),
        };
        assert_eq!(spec.cpus, 1);
        assert_eq!(spec.memory, 512);
        assert_eq!(spec.network.max_connections, 256);
        assert!(spec.network.enabled);
        assert_eq!(spec.upper.size.0, "4Gi");
    }

    #[test]
    fn effective_max_defaults_to_the_effective_value() {
        let mut spec = default_spec();
        spec.cpus = 2;
        spec.memory = 1024;
        assert_eq!(spec.effective_max_cpus(), 2);
        assert_eq!(spec.effective_max_memory(), 1024);
    }

    #[test]
    fn effective_max_uses_the_set_ceiling() {
        let mut spec = default_spec();
        spec.cpus = 1;
        spec.max_cpus = Some(4);
        spec.memory = 512;
        spec.max_memory = Some(2048);
        assert_eq!(spec.effective_max_cpus(), 4);
        assert_eq!(spec.effective_max_memory(), 2048);
    }

    fn default_spec() -> SandboxSpec {
        SandboxSpec {
            image: "python:3.12".to_string(),
            cpus: default_cpus(),
            memory: default_memory_mib(),
            max_cpus: None,
            max_memory: None,
            cmd: vec![],
            entrypoint: vec![],
            env: vec![],
            workdir: None,
            shell: None,
            user: None,
            hostname: None,
            lifecycle: Default::default(),
            max_duration_secs: None,
            idle_timeout_secs: None,
            run_policy: RunPolicy::Once,
            secrets: vec![],
            network: NetworkSpec::default(),
            upper: UpperSpec::default(),
            security_profile: SecurityProfile::default(),
            rlimits: vec![],
            logging: Default::default(),
            desired_state: Default::default(),
        }
    }
}
