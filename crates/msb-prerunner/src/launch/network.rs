//! Mirror of msb's `NetworkConfig` scoped to what the `Sandbox` CRD exposes.
//!
//! The full msb policy engine (`Rule`/`Destination`/`Protocol`/`DestinationGroup`)
//! is intentionally NOT mirrored: the CRD exposes a named policy *preset*, not
//! arbitrary rules, so we translate the 4 presets into msb's `NetworkPolicy` and
//! nothing finer. Every type here is `#[serde(default)]` on msb's side, so we
//! emit only what we set.
//!
//! serde attributes must stay byte-exact with the msb source — especially the
//! kebab-case enums and the string-serialized newtypes.

use std::net::IpAddr;

use msb_crd::SandboxSpec;
use msb_crd::sandbox::{DnsSpec, NetworkSpec, PolicyPreset, PortProtocol as CrdProto};
use serde::Serialize;

/// Guest-facing bind address for published ports. The operator always exposes on
/// the pod IP, so `0.0.0.0` regardless of what the guest requested.
const HOST_BIND_ALL: IpAddr = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);

#[derive(Debug, Clone, Serialize)]
pub struct NetworkConfig {
    pub enabled: bool,
    pub ports: Vec<PublishedPort>,
    pub policy: NetworkPolicy,
    pub dns: DnsConfig,
    pub tls: TlsConfig,
    pub secrets: SecretsConfig,
    pub max_connections: Option<usize>,
    pub trust_host_cas: bool,
    // `interface` is omitted: msb defaults it, and the CRD deliberately does not
    // expose interface overrides (they risk IP conflicts between sandboxes).
}

#[derive(Debug, Clone, Serialize)]
pub struct PublishedPort {
    pub host_port: u16,
    pub guest_port: u16,
    pub protocol: PortProtocol,
    pub host_bind: IpAddr,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Serialize)]
pub struct DnsConfig {
    pub rebind_protection: bool,
    pub nameservers: Vec<Nameserver>,
    pub query_timeout_ms: u64,
}

/// msb serializes each nameserver as a single string (`"1.1.1.1:53"` or
/// `"dns.google:53"`) via a custom impl, never an object.
#[derive(Debug, Clone)]
pub struct Nameserver(String);

impl Serialize for Nameserver {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TlsConfig {
    pub enabled: bool,
    pub intercepted_ports: Vec<u16>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SecretsConfig {
    pub secrets: Vec<SecretEntry>,
}

/// One secret the proxy substitutes. `value` is the resolved plaintext (the
/// prerunner fills it from a k8s Secret); it never enters the guest.
#[derive(Clone, Serialize)]
pub struct SecretEntry {
    pub env_var: String,
    pub value: String,
    pub placeholder: String,
    pub allowed_hosts: Vec<HostPattern>,
    pub require_tls_identity: bool,
}

// Redact `value` in Debug, matching msb's hand-written impl — a resolved secret
// must never reach a log line.
impl std::fmt::Debug for SecretEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretEntry")
            .field("env_var", &self.env_var)
            .field("value", &"<redacted>")
            .field("placeholder", &self.placeholder)
            .field("allowed_hosts", &self.allowed_hosts)
            .finish_non_exhaustive()
    }
}

/// msb serializes this kebab-case, externally tagged: `{"exact":"..."}` /
/// `{"wildcard":"..."}`. (msb also has `Any`, unreachable from CRD host strings.)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostPattern {
    Exact(String),
    Wildcard(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkPolicy {
    pub default_egress: Action,
    pub default_ingress: Action,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rule {
    pub direction: Direction,
    pub destination: Destination,
    pub protocols: Vec<Protocol>,
    pub ports: Vec<PortRange>,
    pub action: Action,
}

impl Rule {
    /// Mirrors msb `allow_dns()`: the `Host` group (not `Any`) is what stops a
    /// guest reaching an arbitrary private resolver on port 53.
    fn allow_dns() -> Self {
        Self {
            direction: Direction::Egress,
            destination: Destination::Group(DestinationGroup::Host),
            protocols: vec![Protocol::Udp, Protocol::Tcp],
            ports: vec![PortRange { start: 53, end: 53 }],
            action: Action::Allow,
        }
    }

    fn allow_egress(group: DestinationGroup) -> Self {
        Self {
            direction: Direction::Egress,
            destination: Destination::Group(group),
            protocols: Vec::new(),
            ports: Vec::new(),
            action: Action::Allow,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
// Only the variants the CRD's policy presets actually emit are mirrored. msb's
// full vocabulary (Ingress/Any directions, arbitrary CIDR/Domain destinations,
// LinkLocal/Metadata/Multicast groups, ICMP protocols) is deliberately omitted —
// the CRD exposes presets, not raw rules. Add a variant when a preset needs it.
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Egress,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    Group(DestinationGroup),
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationGroup {
    /// The node/gateway forwarder (see `Rule::allow_dns`).
    Host,
    Public,
    Private,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl NetworkPolicy {
    fn deny_all() -> Self {
        Self {
            default_egress: Action::Deny,
            default_ingress: Action::Deny,
            rules: Vec::new(),
        }
    }

    fn allow_all() -> Self {
        Self {
            default_egress: Action::Allow,
            default_ingress: Action::Allow,
            rules: Vec::new(),
        }
    }

    // public_only/non_local mirror msb's own constructors rule-for-rule
    // (crates/network/lib/policy/types.rs). They must stay default-DENY egress
    // with explicit allows — a default-allow deny-list would let LinkLocal,
    // Metadata (the cloud metadata IP), and Multicast fall through: an SSRF hole.
    // default_ingress is Allow so published ports keep working.

    fn public_only() -> Self {
        Self {
            default_egress: Action::Deny,
            default_ingress: Action::Allow,
            rules: vec![
                Rule::allow_dns(),
                Rule::allow_egress(DestinationGroup::Public),
            ],
        }
    }

    fn non_local() -> Self {
        Self {
            default_egress: Action::Deny,
            default_ingress: Action::Allow,
            rules: vec![
                Rule::allow_dns(),
                Rule::allow_egress(DestinationGroup::Public),
                Rule::allow_egress(DestinationGroup::Private),
            ],
        }
    }

    fn from_preset(preset: &PolicyPreset) -> Self {
        match preset {
            PolicyPreset::PublicOnly => Self::public_only(),
            PolicyPreset::AllowAll => Self::allow_all(),
            PolicyPreset::DenyAll => Self::deny_all(),
            PolicyPreset::NonLocal => Self::non_local(),
        }
    }
}

/// Pure: `secrets` arrive already resolved (the caller fetched the plaintext).
pub fn network_from_spec(spec: &SandboxSpec, secrets: Vec<SecretEntry>) -> NetworkConfig {
    let net: &NetworkSpec = &spec.network;

    NetworkConfig {
        enabled: net.enabled,
        ports: net
            .published_ports
            .iter()
            .map(|p| PublishedPort {
                host_port: p.container_port,
                guest_port: p.container_port,
                protocol: match p.protocol {
                    CrdProto::Tcp => PortProtocol::Tcp,
                    CrdProto::Udp => PortProtocol::Udp,
                },
                host_bind: HOST_BIND_ALL,
            })
            .collect(),
        policy: NetworkPolicy::from_preset(&net.policy.preset),
        dns: dns_from_spec(&net.dns),
        tls: TlsConfig {
            enabled: net.tls.intercept,
            intercepted_ports: net.tls.intercepted_ports.iter().map(|p| p.port).collect(),
        },
        secrets: SecretsConfig { secrets },
        max_connections: Some(net.max_connections as usize),
        trust_host_cas: net.trust_host_cas,
    }
}

fn dns_from_spec(dns: &DnsSpec) -> DnsConfig {
    DnsConfig {
        rebind_protection: dns.rebind_protection,
        nameservers: dns.nameservers.iter().cloned().map(Nameserver).collect(),
        query_timeout_ms: dns.query_timeout_ms as u64,
    }
}
