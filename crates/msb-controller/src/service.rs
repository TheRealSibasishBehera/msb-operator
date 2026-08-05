//! The per-sandbox ClusterIP Service that fronts the bridge port.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Resource;
use msb_crd::Sandbox;
use sha2::{Digest, Sha256};

use crate::config::{ControllerConfig, SANDBOX_LABEL, SANDBOX_NAME_LABEL};

/// `msb-<sanitized name>-<6 hex of sha256(name)>`: readable, yet always a valid
/// [RFC 1035 Service name] whatever the sandbox name looks like. Keyed on the
/// name alone — a Service is namespaced, so the name is already unique per ns.
///
/// [RFC 1035 Service name]: https://kubernetes.io/docs/concepts/overview/working-with-objects/names/#rfc-1035-label-names
pub fn service_name(name: &str) -> String {
    let hash = hex::encode(Sha256::digest(name.as_bytes()));
    let readable: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(40)
        .collect();
    format!("msb-{}-{}", readable.trim_matches('-'), &hash[..6])
}

/// Builds the ClusterIP Service targeting the sandbox pod's bridge port.
pub fn build(sandbox: &Sandbox, cfg: &ControllerConfig, name: &str, namespace: &str) -> Service {
    let owner = sandbox.controller_owner_ref(&());

    let selector = BTreeMap::from([(SANDBOX_NAME_LABEL.to_string(), name.to_string())]);

    Service {
        metadata: ObjectMeta {
            name: Some(service_name(name)),
            namespace: Some(namespace.to_string()),
            labels: Some(BTreeMap::from([
                (SANDBOX_LABEL.to_string(), "true".to_string()),
                (SANDBOX_NAME_LABEL.to_string(), name.to_string()),
            ])),
            owner_references: owner.map(|o| vec![o]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            selector: Some(selector),
            ports: Some(service_ports(sandbox, cfg)),
            ..Default::default()
        }),
        status: None,
    }
}

/// The first published port that collides with a reserved bridge port, if any.
/// A collision would make a duplicate-port Service the API server rejects.
pub fn reserved_port_conflict(sandbox: &Sandbox, cfg: &ControllerConfig) -> Option<u16> {
    let reserved = [cfg.bridge_port, cfg.bridge_control_port];
    sandbox
        .spec
        .network
        .published_ports
        .iter()
        .map(|p| p.host_port())
        .find(|host| reserved.contains(&i32::from(*host)))
}

/// The bridge port, the bridge's control port, and every published port.
fn service_ports(sandbox: &Sandbox, cfg: &ControllerConfig) -> Vec<ServicePort> {
    let mut ports = vec![
        ServicePort {
            name: Some("agent".to_string()),
            port: cfg.bridge_port,
            target_port: Some(IntOrString::Int(cfg.bridge_port)),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        },
        ServicePort {
            name: Some("control".to_string()),
            port: cfg.bridge_control_port,
            target_port: Some(IntOrString::Int(cfg.bridge_control_port)),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        },
    ];
    for p in &sandbox.spec.network.published_ports {
        let host = i32::from(p.host_port());
        ports.push(ServicePort {
            name: Some(crate::pod::port_name(p.host_port())),
            port: host,
            target_port: Some(IntOrString::Int(host)),
            protocol: Some(
                match p.protocol {
                    msb_crd::sandbox::PortProtocol::Tcp => "TCP",
                    msb_crd::sandbox::PortProtocol::Udp => "UDP",
                }
                .to_string(),
            ),
            ..Default::default()
        });
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_is_rfc1035_safe() {
        // Names that would break Service admission if used raw: digit-led, illegal
        // characters, over-long.
        for raw in ["1-x", "My_Sandbox!", &"z".repeat(80)] {
            let n = service_name(raw);
            assert!(n.starts_with("msb-"));
            assert!(n.len() <= 63);
            assert!(n.chars().next().unwrap().is_ascii_alphabetic());
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        }
    }

    #[test]
    fn service_name_carries_the_readable_sandbox_name() {
        assert!(service_name("e2e-ports").starts_with("msb-e2e-ports-"));
    }

    #[test]
    fn reserved_port_conflict_catches_bridge_ports_only() {
        use crate::pod::test_support::{config, sandbox_with_ports};
        let cfg = config();
        assert_eq!(
            reserved_port_conflict(&sandbox_with_ports(&[8080]), &cfg),
            Some(8080)
        );
        assert_eq!(
            reserved_port_conflict(&sandbox_with_ports(&[7000]), &cfg),
            Some(7000)
        );
        assert_eq!(
            reserved_port_conflict(&sandbox_with_ports(&[9090]), &cfg),
            None
        );
        assert_eq!(reserved_port_conflict(&sandbox_with_ports(&[]), &cfg), None);
    }

    #[test]
    fn service_name_is_deterministic_and_distinct() {
        assert_eq!(service_name("a"), service_name("a"));
        assert_ne!(service_name("a"), service_name("b"));
        // Distinct even when the readable part sanitizes to the same string.
        assert_ne!(service_name("a.b"), service_name("a-b"));
    }

    #[test]
    fn service_targets_the_bridge_port_and_selects_the_pod() {
        use crate::pod::test_support::{config, sandbox};
        let svc = build(&sandbox(), &config(), "my-sandbox", "team-a");
        let spec = svc.spec.as_ref().unwrap();
        assert_eq!(spec.type_.as_deref(), Some("ClusterIP"));
        assert_eq!(
            spec.selector.as_ref().unwrap().get(SANDBOX_NAME_LABEL),
            Some(&"my-sandbox".to_string())
        );
        // The sandbox name is also a Service label, so `kubectl get svc -L` maps
        // the derived name back to its sandbox.
        assert_eq!(
            svc.metadata
                .labels
                .as_ref()
                .unwrap()
                .get(SANDBOX_NAME_LABEL),
            Some(&"my-sandbox".to_string())
        );
        let port = &spec.ports.as_ref().unwrap()[0];
        assert_eq!(port.port, 7000);

        // Owner-ref'd to the Sandbox so the Service GCs on delete.
        let owners = svc.metadata.owner_references.as_ref().unwrap();
        assert_eq!(owners[0].kind, "Sandbox");
        assert_eq!(owners[0].controller, Some(true));
    }

    #[test]
    fn published_ports_are_added_alongside_the_bridge_port() {
        use crate::pod::test_support::{config, sandbox_with_ports};
        let svc = build(
            &sandbox_with_ports(&[8000]),
            &config(),
            "my-sandbox",
            "team-a",
        );
        let ports = svc.spec.as_ref().unwrap().ports.as_ref().unwrap();
        assert_eq!(ports.len(), 3, "bridge + control + one published");
        assert_eq!(ports[0].name.as_deref(), Some("agent"));
        assert_eq!(ports[1].name.as_deref(), Some("control"));
        assert_eq!(ports[2].name.as_deref(), Some("port-8000"));
        assert_eq!(ports[2].port, 8000);
        assert_eq!(ports[2].target_port, Some(IntOrString::Int(8000)));
    }

    #[test]
    fn service_exposes_the_bridge_control_port() {
        use crate::pod::test_support::{config, sandbox};
        let svc = build(&sandbox(), &config(), "my-sandbox", "team-a");
        let ports = svc.spec.as_ref().unwrap().ports.as_ref().unwrap();
        let control = ports
            .iter()
            .find(|p| p.name.as_deref() == Some("control"))
            .unwrap();
        assert_eq!(control.port, 8080);
        assert_eq!(control.target_port, Some(IntOrString::Int(8080)));
    }
}
