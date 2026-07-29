//! The per-sandbox ClusterIP Service that fronts the bridge port.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Resource;
use msb_crd::Sandbox;
use sha2::{Digest, Sha256};

use crate::config::{ControllerConfig, SANDBOX_LABEL};

/// The Service name for a sandbox: `msb-<12 hex of sha256(namespace/name)>`.
/// Derived (not the sandbox name) because Service names are RFC 1035 — max 63
/// chars, must start with a letter — while sandbox names may start with a digit
/// or exceed 63; a raw name would be admitted by the API server then rejected at
/// Service creation, a hot loop. The `msb-` prefix + hash is always valid, and
/// deterministic so we can recompute it without storing extra state.
pub fn service_name(namespace: &str, name: &str) -> String {
    let hash = hex::encode(Sha256::digest(format!("{namespace}/{name}").as_bytes()));
    format!("msb-{}", &hash[..12])
}

/// Builds the ClusterIP Service targeting the sandbox pod's bridge port.
pub fn build(sandbox: &Sandbox, cfg: &ControllerConfig, name: &str, namespace: &str) -> Service {
    let owner = sandbox.controller_owner_ref(&());

    let selector = BTreeMap::from([(
        "microsandbox.dev/sandbox-name".to_string(),
        name.to_string(),
    )]);

    Service {
        metadata: ObjectMeta {
            name: Some(service_name(namespace, name)),
            namespace: Some(namespace.to_string()),
            labels: Some(BTreeMap::from([(
                SANDBOX_LABEL.to_string(),
                "true".to_string(),
            )])),
            owner_references: owner.map(|o| vec![o]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            selector: Some(selector),
            ports: Some(vec![ServicePort {
                name: Some("agent".to_string()),
                port: cfg.bridge_port,
                target_port: Some(IntOrString::Int(cfg.bridge_port)),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_is_rfc1035_safe() {
        // A name that would break Service admission if used raw: starts with a
        // digit. The derived name is always msb-<hex>.
        let n = service_name("team-a", "1-x");
        assert!(n.starts_with("msb-"));
        assert!(n.len() <= 63);
        assert!(n.chars().next().unwrap().is_ascii_alphabetic());
        let hash = n.strip_prefix("msb-").unwrap();
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn service_name_is_deterministic_and_distinct() {
        assert_eq!(service_name("ns", "a"), service_name("ns", "a"));
        assert_ne!(service_name("ns", "a"), service_name("ns", "b"));
        // Namespace is part of the key, so same name in two namespaces differs.
        assert_ne!(service_name("ns1", "a"), service_name("ns2", "a"));
    }

    #[test]
    fn service_targets_the_bridge_port_and_selects_the_pod() {
        use crate::pod::test_support::{config, sandbox};
        let svc = build(&sandbox(), &config(), "my-sandbox", "team-a");
        let spec = svc.spec.as_ref().unwrap();
        assert_eq!(spec.type_.as_deref(), Some("ClusterIP"));
        assert_eq!(
            spec.selector.as_ref().unwrap().get("microsandbox.dev/sandbox-name"),
            Some(&"my-sandbox".to_string())
        );
        let port = &spec.ports.as_ref().unwrap()[0];
        assert_eq!(port.port, 7000);

        // Owner-ref'd to the Sandbox so the Service GCs on delete.
        let owners = svc.metadata.owner_references.as_ref().unwrap();
        assert_eq!(owners[0].kind, "Sandbox");
        assert_eq!(owners[0].controller, Some(true));
    }
}
