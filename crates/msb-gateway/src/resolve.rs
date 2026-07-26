//! Resolve a Sandbox CRD name to its bridge WebSocket endpoint (off `status`, no
//! registry — the API server holds the state).

use kube::{Api, Client};
use msb_crd::{Sandbox, SandboxPhase};

use crate::error::GatewayError;

const BRIDGE_PORT: u16 = 7000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeTarget {
    pub url: String,
}

// Plaintext `ws://` is fine: the hop is in-cluster; client-facing TLS is the
// gateway's own listener, not this leg.
pub fn bridge_url(service_name: &str, namespace: &str) -> String {
    format!("ws://{service_name}.{namespace}.svc.cluster.local:{BRIDGE_PORT}/")
}

pub async fn resolve(
    client: &Client,
    namespace: &str,
    name: &str,
) -> Result<BridgeTarget, GatewayError> {
    let api: Api<Sandbox> = Api::namespaced(client.clone(), namespace);
    let sandbox = match api.get_opt(name).await? {
        Some(sb) => sb,
        None => return Err(GatewayError::NotFound(name.to_string())),
    };
    target_from_sandbox(&sandbox, namespace, name)
}

// Split from `resolve` so the phase gating and URL building are unit-testable.
pub fn target_from_sandbox(
    sandbox: &Sandbox,
    namespace: &str,
    name: &str,
) -> Result<BridgeTarget, GatewayError> {
    let status = sandbox
        .status
        .as_ref()
        .ok_or_else(|| GatewayError::NotReady(name.to_string()))?;

    // Only a Running sandbox has a live bridge to reach.
    match status.phase {
        Some(SandboxPhase::Running) => {}
        _ => return Err(GatewayError::NotReady(name.to_string())),
    }

    let service_name = status
        .service_name
        .as_deref()
        .ok_or_else(|| GatewayError::NotReady(name.to_string()))?;

    Ok(BridgeTarget {
        url: bridge_url(service_name, namespace),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use msb_crd::{Sandbox, SandboxStatus};

    fn sandbox_with(status: Option<SandboxStatus>) -> Sandbox {
        // Build via JSON so we don't have to spell out every required spec field;
        // resolution only reads `.status`, so a minimal valid spec suffices.
        let mut sb: Sandbox = serde_json::from_value(serde_json::json!({
            "apiVersion": "sandbox.microsandbox.io/v1alpha1",
            "kind": "Sandbox",
            "metadata": { "name": "s1", "namespace": "ns" },
            "spec": { "image": "alpine:3.20" },
        }))
        .expect("valid minimal Sandbox");
        sb.status = status;
        sb
    }

    #[test]
    fn bridge_url_is_cluster_dns_on_bridge_port() {
        assert_eq!(
            bridge_url("msb-abc123", "team-a"),
            "ws://msb-abc123.team-a.svc.cluster.local:7000/"
        );
    }

    #[test]
    fn running_with_service_name_resolves() {
        let sb = sandbox_with(Some(SandboxStatus {
            phase: Some(SandboxPhase::Running),
            service_name: Some("msb-deadbeef".into()),
            ..Default::default()
        }));
        let t = target_from_sandbox(&sb, "ns", "s1").unwrap();
        assert_eq!(t.url, "ws://msb-deadbeef.ns.svc.cluster.local:7000/");
    }

    #[test]
    fn pending_is_not_ready() {
        let sb = sandbox_with(Some(SandboxStatus {
            phase: Some(SandboxPhase::Pending),
            service_name: Some("msb-deadbeef".into()),
            ..Default::default()
        }));
        assert!(matches!(
            target_from_sandbox(&sb, "ns", "s1"),
            Err(GatewayError::NotReady(_))
        ));
    }

    #[test]
    fn no_status_is_not_ready() {
        let sb = sandbox_with(None);
        assert!(matches!(
            target_from_sandbox(&sb, "ns", "s1"),
            Err(GatewayError::NotReady(_))
        ));
    }

    #[test]
    fn running_without_service_name_is_not_ready() {
        let sb = sandbox_with(Some(SandboxStatus {
            phase: Some(SandboxPhase::Running),
            service_name: None,
            ..Default::default()
        }));
        assert!(matches!(
            target_from_sandbox(&sb, "ns", "s1"),
            Err(GatewayError::NotReady(_))
        ));
    }
}
