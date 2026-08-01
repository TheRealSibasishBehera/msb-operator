//! Pure wire-type <-> `Sandbox` CRD translation (no cluster access, unit-testable).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use kube::api::ObjectMeta;
use microsandbox_types::{
    CloudCreateSandboxRequest, CloudCreateSandboxResponse, CloudRootfsSource, CloudRlimit,
    CloudRlimitResource, CloudSandboxStatus, SecurityProfile as WireSecurityProfile,
};
use msb_crd::sandbox::{Rlimit, RlimitResource, SecurityProfile};
use msb_crd::{Sandbox, SandboxPhase, SandboxSpec, SandboxStatus};

use crate::error::GatewayError;

/// Annotation prefix for wire fields with no CRD spec home (round-tripped for `get`).
pub const CLOUD_ANN: &str = "microsandbox.dev/cloud-";

/// Annotation prefix for msb labels that aren't valid k8s labels (preserved, not selectable).
pub const CLOUD_LABEL_ANN: &str = "microsandbox.dev/cloud-label.";

/// Marks a Sandbox as gateway-created. msb reserves the `sandbox.`/`microsandbox.`
/// key prefixes, so a client can never set (and thus never shadow) this.
pub const MANAGED_BY_KEY: &str = "sandbox.microsandbox.dev/managed-by";
pub const MANAGED_BY_VALUE: &str = "msb-gateway";

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

/// A k8s label key: an optional `<dns-subdomain>/` prefix then a ≤63-char name
/// segment bounded by alphanumerics with interior `-_.`.
fn is_k8s_label_key(key: &str) -> bool {
    let name = match key.split_once('/') {
        Some((prefix, name)) => {
            if prefix.is_empty() || prefix.len() > 253 {
                return false;
            }
            name
        }
        None => key,
    };
    is_k8s_label_segment(name)
}

/// A k8s label value or name segment: empty, or ≤63 chars bounded by alphanumerics
/// with interior `-_.`.
fn is_k8s_label_segment(v: &str) -> bool {
    if v.is_empty() {
        return true;
    }
    v.len() <= 63
        && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && v.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && v.bytes().last().is_some_and(|b| b.is_ascii_alphanumeric())
}

/// The CRD object a create request maps to: its spec plus the `metadata.labels`
/// and `metadata.annotations` that carry the wire fields with no spec home.
pub struct SpecMapping {
    pub spec: SandboxSpec,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
}

/// Cloud create request -> CRD spec, k8s labels, and annotations for fields with no
/// spec home. Labels that are valid k8s labels become selectable `metadata.labels`;
/// the rest (msb labels are free-form) are preserved as `cloud-label.*` annotations.
pub fn request_to_spec(req: &CloudCreateSandboxRequest) -> Result<SpecMapping, GatewayError> {
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
        // msb's `cmd` is the argv (the common override); `entrypoint` overrides the
        // image entrypoint. Map each to its own CRD field.
        cmd: spec.runtime.cmd.clone().unwrap_or_default(),
        entrypoint: spec.runtime.entrypoint.clone().unwrap_or_default(),
        env: spec
            .env
            .iter()
            .map(|e| msb_crd::sandbox::EnvVar {
                name: e.key.clone(),
                value: e.value.clone(),
            })
            .collect(),
        workdir: spec.runtime.workdir.clone(),
        shell: spec.runtime.shell.clone(),
        user: spec.runtime.user.clone(),
        hostname: None,
        ephemeral: spec.lifecycle.ephemeral,
        max_duration_secs: spec.lifecycle.max_duration_secs,
        idle_timeout_secs: spec.lifecycle.idle_timeout_secs,
        run_policy: Default::default(),
        secrets: Vec::new(),
        network: Default::default(),
        upper: Default::default(),
        security_profile: map_security(spec.security_profile),
        rlimits: map_rlimits(&spec.rlimits),
        logging: Default::default(),
        desired_state: Default::default(),
    };

    let mut ann = BTreeMap::new();
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

    let mut labels = BTreeMap::new();
    for (k, v) in &spec.labels {
        if is_k8s_label_key(k) && is_k8s_label_segment(v) {
            labels.insert(k.clone(), v.clone());
        } else {
            ann.insert(format!("{CLOUD_LABEL_ANN}{k}"), v.clone());
        }
    }
    labels.insert(MANAGED_BY_KEY.to_string(), MANAGED_BY_VALUE.to_string());

    Ok(SpecMapping {
        spec: sandbox_spec,
        labels,
        annotations: ann,
    })
}

/// Translate msb's `?labels=` param (a JSON object of `key=value`) into a k8s
/// equality label selector (`k1=v1,k2=v2`). `None`/empty means no filter.
/// Errors on malformed JSON so a client's typo doesn't silently return everything.
pub fn labels_query_to_selector(labels: Option<&str>) -> Result<Option<String>, GatewayError> {
    let raw = match labels {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };
    let map: BTreeMap<String, String> = serde_json::from_str(raw).map_err(|e| {
        GatewayError::InvalidRequest(format!("labels filter must be a JSON object: {e}"))
    })?;
    if map.is_empty() {
        return Ok(None);
    }
    let sel = map
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(Some(sel))
}

/// Wire security profile -> CRD. Previously dropped, so a `Restricted` request
/// silently ran `Default` (fail-open); now it reaches the guest.
fn map_security(p: WireSecurityProfile) -> SecurityProfile {
    match p {
        WireSecurityProfile::Default => SecurityProfile::Default,
        WireSecurityProfile::Restricted => SecurityProfile::Restricted,
    }
}

fn map_rlimits(rlimits: &[CloudRlimit]) -> Vec<Rlimit> {
    rlimits
        .iter()
        .map(|r| Rlimit {
            resource: map_rlimit_resource(r.resource),
            soft: r.soft,
            hard: r.hard,
        })
        .collect()
}

fn map_rlimit_resource(r: CloudRlimitResource) -> RlimitResource {
    use CloudRlimitResource as W;
    match r {
        W::Cpu => RlimitResource::Cpu,
        W::Fsize => RlimitResource::Fsize,
        W::Data => RlimitResource::Data,
        W::Stack => RlimitResource::Stack,
        W::Core => RlimitResource::Core,
        W::Rss => RlimitResource::Rss,
        W::Nproc => RlimitResource::Nproc,
        W::Nofile => RlimitResource::Nofile,
        W::Memlock => RlimitResource::Memlock,
        W::As => RlimitResource::As,
        W::Locks => RlimitResource::Locks,
        W::Sigpending => RlimitResource::Sigpending,
        W::Msgqueue => RlimitResource::Msgqueue,
        W::Nice => RlimitResource::Nice,
        W::Rtprio => RlimitResource::Rtprio,
        W::Rttime => RlimitResource::Rttime,
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
        Some(SandboxPhase::Stopped) | Some(SandboxPhase::Succeeded) => CloudSandboxStatus::Stopped,
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
                    shell: Some("/bin/bash".into()),
                    user: Some("appuser".into()),
                    entrypoint: Some(vec!["/entry".into()]),
                    cmd: Some(vec!["sleep".into(), "300".into()]),
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
    fn structured_fields_map_to_spec() {
        let SpecMapping { spec, .. } = request_to_spec(&req()).unwrap();
        assert_eq!(spec.image, "alpine:3.20");
        assert_eq!(spec.cpus, 2);
        assert_eq!(spec.memory, 1024);
        // Wire cmd -> CRD cmd; wire entrypoint -> CRD entrypoint (no longer swapped).
        assert_eq!(spec.cmd, vec!["sleep", "300"]);
        assert_eq!(spec.entrypoint, vec!["/entry"]);
        assert_eq!(spec.workdir.as_deref(), Some("/app"));
        assert_eq!(spec.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(spec.user.as_deref(), Some("appuser"));
        assert_eq!(spec.env, vec![msb_crd::sandbox::EnvVar { name: "K".into(), value: "V".into() }]);
        assert!(spec.ephemeral);
        assert_eq!(spec.max_duration_secs, Some(600));
    }

    #[test]
    fn k8s_valid_labels_map_to_metadata_labels() {
        let mut r = req();
        r.spec.labels.insert("app".into(), "web".into());
        r.spec.labels.insert("team.example.com/tier".into(), "frontend".into());
        let SpecMapping { labels, annotations: ann, .. } = request_to_spec(&r).unwrap();
        assert_eq!(labels.get("app").unwrap(), "web");
        assert_eq!(labels.get("team.example.com/tier").unwrap(), "frontend");
        // No cloud-label.* annotation for a label that fit metadata.labels.
        assert!(!ann.keys().any(|k| k.starts_with(CLOUD_LABEL_ANN)));
    }

    #[test]
    fn free_form_labels_fall_back_to_annotations() {
        let mut r = req();
        // Value too long / illegal charset for a k8s label — preserved, not dropped.
        r.spec.labels.insert("note".into(), "a value with spaces".into());
        let SpecMapping { labels, annotations: ann, .. } = request_to_spec(&r).unwrap();
        assert!(!labels.contains_key("note"));
        assert_eq!(
            ann.get("microsandbox.dev/cloud-label.note").unwrap(),
            "a value with spaces"
        );
    }

    #[test]
    fn managed_by_label_is_always_stamped() {
        let SpecMapping { labels, .. } = request_to_spec(&req()).unwrap();
        assert_eq!(labels.get(MANAGED_BY_KEY).unwrap(), MANAGED_BY_VALUE);
    }

    #[test]
    fn security_and_rlimits_map_through() {
        let mut r = req();
        r.spec.security_profile = WireSecurityProfile::Restricted;
        r.spec.rlimits = vec![CloudRlimit {
            resource: CloudRlimitResource::Nofile,
            soft: 1024,
            hard: 2048,
        }];
        let SpecMapping { spec, .. } = request_to_spec(&r).unwrap();
        assert_eq!(spec.security_profile, SecurityProfile::Restricted);
        assert_eq!(spec.rlimits.len(), 1);
        assert_eq!(spec.rlimits[0].resource, RlimitResource::Nofile);
        assert_eq!((spec.rlimits[0].soft, spec.rlimits[0].hard), (1024, 2048));
    }

    #[test]
    fn labels_query_becomes_a_k8s_selector() {
        assert_eq!(labels_query_to_selector(None).unwrap(), None);
        assert_eq!(labels_query_to_selector(Some("")).unwrap(), None);
        assert_eq!(labels_query_to_selector(Some("{}")).unwrap(), None);
        assert_eq!(
            labels_query_to_selector(Some(r#"{"app":"web"}"#)).unwrap(),
            Some("app=web".to_string())
        );
        // BTreeMap ordering makes the selector deterministic.
        assert_eq!(
            labels_query_to_selector(Some(r#"{"b":"2","a":"1"}"#)).unwrap(),
            Some("a=1,b=2".to_string())
        );
        assert!(labels_query_to_selector(Some("not-json")).is_err());
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
        assert!(matches!(phase_to_status(Some(SandboxPhase::Stopped), false, true), Stopped));
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
        // Simulate what create writes, then map back: the response carries
        // lifecycle state derived from spec + metadata.
        let SpecMapping { spec, labels, annotations: ann } = request_to_spec(&req()).unwrap();
        let sb = Sandbox {
            metadata: ObjectMeta {
                name: Some("my-sb".into()),
                namespace: Some("ns".into()),
                labels: Some(labels.into_iter().collect()),
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
