//! Pure wire-type <-> `Sandbox` CRD translation (no cluster access, unit-testable).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use kube::api::ObjectMeta;
use microsandbox_types::{
    CloudCreateSandboxRequest, CloudCreateSandboxResponse, CloudRootfsSource, CloudSandboxStatus,
};
use msb_crd::{Sandbox, SandboxPhase, SandboxSpec, SandboxStatus};

use crate::error::GatewayError;

/// Annotation prefix for wire fields with no CRD spec home (round-tripped for `get`).
pub const CLOUD_ANN: &str = "microsandbox.dev/cloud-";

/// Reject a name that isn't a valid k8s object name (SDK names are freer than ours).
pub fn validate_name(name: &str) -> Result<(), GatewayError> {
    let ok = !name.is_empty()
        && name.len() <= 253
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(GatewayError::InvalidRequest(format!(
            "sandbox name {name:?} must be a valid Kubernetes name: lowercase alphanumeric and '-'"
        )))
    }
}

/// Cloud create request -> CRD spec + annotations for fields the spec can't hold.
/// `env` is stashed for echo only, NOT injected into the guest (a V1 limitation).
pub fn request_to_spec(
    req: &CloudCreateSandboxRequest,
) -> Result<(SandboxSpec, BTreeMap<String, String>), GatewayError> {
    let spec = &req.spec;
    validate_name(&spec.name)?;

    // Our CRD image is a plain OCI reference; the host-path variants have no
    // representation in our model (and would be a host-access escape).
    let image = match &spec.image {
        CloudRootfsSource::Oci { reference } => reference.clone(),
        _ => {
            return Err(GatewayError::InvalidRequest(
                "only OCI image references are supported".into(),
            ))
        }
    };

    let sandbox_spec = SandboxSpec {
        image,
        cpus: spec.resources.vcpus as u32,
        memory: spec.resources.memory_mib,
        cmd: spec.runtime.entrypoint.clone().unwrap_or_default(),
        ephemeral: spec.lifecycle.ephemeral,
        max_duration_secs: spec.lifecycle.max_duration_secs,
        idle_timeout_secs: spec.lifecycle.idle_timeout_secs,
        run_policy: Default::default(),
        secrets: Vec::new(),
        network: Default::default(),
        upper: Default::default(),
        volumes: Vec::new(),
    };

    let mut ann = BTreeMap::new();
    // env: no CRD spec field → annotation for echo. NOT injected into the guest.
    if !spec.env.is_empty() {
        if let Ok(j) = serde_json::to_string(&spec.env) {
            ann.insert(format!("{CLOUD_ANN}env"), j);
        }
    }
    stash(&mut ann, "workdir", spec.runtime.workdir.as_ref());
    stash(&mut ann, "shell", spec.runtime.shell.as_ref());
    stash(&mut ann, "user", spec.runtime.user.as_ref());
    if let Some(lvl) = spec.runtime.log_level {
        if let Ok(j) = serde_json::to_string(&lvl) {
            ann.insert(format!("{CLOUD_ANN}log-level"), j.trim_matches('"').to_string());
        }
    }
    if !spec.runtime.scripts.is_empty() {
        if let Ok(j) = serde_json::to_string(&spec.runtime.scripts) {
            ann.insert(format!("{CLOUD_ANN}scripts"), j);
        }
    }
    Ok((sandbox_spec, ann))
}

fn stash(ann: &mut BTreeMap<String, String>, key: &str, val: Option<&String>) {
    if let Some(v) = val {
        ann.insert(format!("{CLOUD_ANN}{key}"), v.clone());
    }
}

/// CRD phase -> the six-variant wire status.
pub fn phase_to_status(
    phase: Option<SandboxPhase>,
    terminating: bool,
    has_started: bool,
) -> CloudSandboxStatus {
    if terminating {
        return CloudSandboxStatus::Stopping;
    }
    match phase {
        Some(SandboxPhase::Running) => CloudSandboxStatus::Running,
        Some(SandboxPhase::Succeeded) => CloudSandboxStatus::Stopped,
        Some(SandboxPhase::Failed) => CloudSandboxStatus::Failed,
        _ if has_started => CloudSandboxStatus::Starting,
        _ => CloudSandboxStatus::Created,
    }
}

/// Sandbox CRD -> a fully-populated `CloudCreateSandboxResponse`. All required
/// fields derive from metadata/status, so a raw-`kubectl` Sandbox (no `cloud-*`
/// annotations) still decodes.
pub fn sandbox_to_cloud(sb: &Sandbox, _namespace: &str) -> CloudCreateSandboxResponse {
    let meta = &sb.metadata;
    let name = meta.name.clone().unwrap_or_default();
    let status = sb.status.clone().unwrap_or_default();

    let terminating = meta.deletion_timestamp.is_some();
    let has_started = status.started_at.is_some()
        || matches!(status.phase, Some(SandboxPhase::Running));

    let wire_status = phase_to_status(status.phase.clone(), terminating, has_started);
    let started_at = parse_ts(status.started_at.as_deref());
    let stopped_at = parse_ts(status.terminated_at.as_deref());
    let last_error = last_error(&status);

    CloudCreateSandboxResponse {
        id: name.clone(), // we collapse id == name; a closed loop we own both ends of
        org_id: String::new(),
        slug: name.clone(),
        name,
        status: wire_status,
        status_reason: None,
        // The server-owned resolved-spec projection; the SDK never reconstructs
        // the request from it, so we omit it.
        spec: None,
        ephemeral: sb.spec.ephemeral,
        created_at: meta_created_at(meta),
        started_at,
        stopped_at,
        last_failure_message: last_error,
    }
}

fn last_error(status: &SandboxStatus) -> Option<String> {
    status.termination_reason.as_ref().and_then(|r| {
        // Only surface non-clean terminations as an error.
        if r.is_clean() {
            None
        } else {
            Some(format!("{r:?}"))
        }
    })
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn meta_created_at(meta: &ObjectMeta) -> DateTime<Utc> {
    // k8s-openapi's `Time` wraps a jiff::Timestamp; format to RFC3339 and parse
    // into chrono to avoid a jiff↔chrono dependency bridge.
    meta.creation_timestamp
        .as_ref()
        .and_then(|t| DateTime::parse_from_rfc3339(&t.0.to_string()).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use microsandbox_types::{
        CloudSandboxResources, CloudSandboxRuntimeOptions, CloudSandboxSpec, EnvVar, SandboxPolicy,
    };

    fn req() -> CloudCreateSandboxRequest {
        CloudCreateSandboxRequest {
            spec: CloudSandboxSpec {
                name: "my-sb".into(),
                image: CloudRootfsSource::Oci {
                    reference: "alpine:3.20".into(),
                },
                resources: CloudSandboxResources {
                    vcpus: 2,
                    memory_mib: 1024,
                    disk_size_mib: None,
                },
                runtime: CloudSandboxRuntimeOptions {
                    workdir: Some("/app".into()),
                    entrypoint: Some(vec!["sleep".into(), "300".into()]),
                    ..Default::default()
                },
                env: vec![EnvVar {
                    key: "K".into(),
                    value: "V".into(),
                }],
                lifecycle: SandboxPolicy {
                    ephemeral: true,
                    max_duration_secs: Some(600),
                    idle_timeout_secs: None,
                },
                ..Default::default()
            },
        }
    }

    fn sandbox_from_json(v: serde_json::Value) -> Sandbox {
        serde_json::from_value(v).expect("valid Sandbox")
    }

    #[test]
    fn name_validation_rejects_uppercase_and_bad_chars() {
        assert!(validate_name("my-sb-1").is_ok());
        assert!(validate_name("Agent_1").is_err());
        assert!(validate_name("-lead").is_err());
        assert!(validate_name("trail-").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn structured_fields_map_to_spec_and_rest_to_annotations() {
        let (spec, ann) = request_to_spec(&req()).unwrap();
        assert_eq!(spec.image, "alpine:3.20");
        assert_eq!(spec.cpus, 2);
        assert_eq!(spec.memory, 1024);
        assert_eq!(spec.cmd, vec!["sleep", "300"]);
        assert!(spec.ephemeral);
        assert_eq!(spec.max_duration_secs, Some(600));
        assert!(!ann.contains_key("microsandbox.dev/cloud-max-duration-secs"));
        assert!(ann.contains_key("microsandbox.dev/cloud-env"));
        assert_eq!(ann.get("microsandbox.dev/cloud-workdir").unwrap(), "/app");
        assert!(!ann.contains_key("microsandbox.dev/cloud-shell"));
    }

    #[test]
    fn bad_name_rejected_by_request_to_spec() {
        let mut r = req();
        r.spec.name = "BAD_NAME".into();
        assert!(matches!(request_to_spec(&r), Err(GatewayError::InvalidRequest(_))));
    }

    #[test]
    fn status_maps_all_six_variants() {
        use CloudSandboxStatus::*;
        assert!(matches!(phase_to_status(Some(SandboxPhase::Running), false, true), Running));
        assert!(matches!(phase_to_status(Some(SandboxPhase::Succeeded), false, true), Stopped));
        assert!(matches!(phase_to_status(Some(SandboxPhase::Failed), false, true), Failed));
        // terminating overrides everything
        assert!(matches!(phase_to_status(Some(SandboxPhase::Running), true, true), Stopping));
        // Pending + started = Starting; Pending + not started = Created
        assert!(matches!(phase_to_status(Some(SandboxPhase::Pending), false, true), Starting));
        assert!(matches!(phase_to_status(Some(SandboxPhase::Pending), false, false), Created));
    }

    #[test]
    fn raw_kubectl_sandbox_with_no_annotations_still_maps_cleanly() {
        // A Sandbox created directly (no cloud-* annotations, no status): the
        // response must still be fully populated so list/get decode.
        let sb = sandbox_from_json(serde_json::json!({
            "apiVersion": "sandbox.microsandbox.dev/v1alpha1",
            "kind": "Sandbox",
            "metadata": { "name": "raw-sb", "namespace": "team-a" },
            "spec": { "image": "alpine:3.20", "cpus": 1, "memory": 512, "cmd": [], "ephemeral": false },
        }));
        let cloud = sandbox_to_cloud(&sb, "team-a");
        assert_eq!(cloud.id, "raw-sb");
        assert_eq!(cloud.name, "raw-sb");
        assert_eq!(cloud.slug, "raw-sb");
        assert!(cloud.spec.is_none()); // server-owned projection, we omit it
        assert!(!cloud.ephemeral);
        assert!(matches!(cloud.status, CloudSandboxStatus::Created)); // no phase, not started
    }

    #[test]
    fn gateway_created_sandbox_maps_to_response() {
        // Simulate what create writes: spec + annotations, then map back. The
        // echoed fields live in annotations; the response carries lifecycle state.
        let (spec, ann) = request_to_spec(&req()).unwrap();
        assert_eq!(
            ann.get("microsandbox.dev/cloud-env").unwrap(),
            &serde_json::to_string(&vec![EnvVar {
                key: "K".into(),
                value: "V".into()
            }])
            .unwrap()
        );
        assert_eq!(ann.get("microsandbox.dev/cloud-workdir").unwrap(), "/app");
        let sb = Sandbox {
            metadata: ObjectMeta {
                name: Some("my-sb".into()),
                namespace: Some("ns".into()),
                annotations: Some(ann.into_iter().collect()),
                ..Default::default()
            },
            spec,
            status: None,
        };
        let cloud = sandbox_to_cloud(&sb, "ns");
        assert_eq!(cloud.name, "my-sb");
        assert_eq!(cloud.id, "my-sb");
        assert!(cloud.ephemeral);
        assert!(cloud.spec.is_none());
    }
}
