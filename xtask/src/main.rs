use anyhow::{Context, Result, bail};
use k8s_openapi::api::rbac::v1::{ClusterRole, PolicyRule};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::ValidationRule;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::CustomResourceExt;
use msb_crd::Sandbox;
use std::{fs, path::Path};

mod api_docs;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("generate-crd") => generate_crd(),
        Some("generate-rbac") => generate_rbac(),
        Some("generate-api-docs") => api_docs::generate(),
        Some(cmd) => bail!("unknown command: {cmd}"),
        None => bail!(
            "usage: cargo xtask <command>\n  \
             generate-crd       write deploy/crds/sandboxes.yaml\n  \
             generate-rbac      write deploy/rbac/controller-clusterrole.yaml\n  \
             generate-api-docs  write userdocs/reference/sandbox-{{spec,status}}.mdx"
        ),
    }
}

/// The controller ServiceAccount's cluster-wide permissions.
///
/// This list is the source of truth for controller RBAC. Each rule maps to a
/// concrete `Api` call in `msb-controller`; review the two together. Nothing
/// grants Secret access — the kubelet mounts referenced Secrets and the runtime
/// reads them from the volume, so no operator Secret RBAC is needed.
fn controller_policy_rules() -> Vec<PolicyRule> {
    let rule = |api_groups: &[&str], resources: &[&str], verbs: &[&str]| PolicyRule {
        api_groups: Some(api_groups.iter().map(|s| s.to_string()).collect()),
        resources: Some(resources.iter().map(|s| s.to_string()).collect()),
        verbs: verbs.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };

    vec![
        // Sandboxes: watched by the Controller, read and deleted on reconcile.
        rule(
            &["sandbox.microsandbox.dev"],
            &["sandboxes"],
            &["get", "list", "watch", "delete"],
        ),
        // Status subresource: only ever patch_status'd, never GET separately.
        rule(
            &["sandbox.microsandbox.dev"],
            &["sandboxes/status"],
            &["patch"],
        ),
        // Pods: created and updated via SSA (patch), watched via .owns(),
        // read and deleted on exit. No plain update/replace path.
        rule(
            &[""],
            &["pods"],
            &["get", "list", "watch", "create", "patch", "delete"],
        ),
        // Services: the per-sandbox ClusterIP fronting the bridge, applied via
        // SSA. Owner-ref'd to the Sandbox, so GC handles deletion.
        rule(&[""], &["services"], &["get", "create", "patch"]),
        // Leases: leader election. kube-leader-election creates the Lease once
        // then renews/acquires/steps-down via server-side apply (PATCH), so
        // `patch` is required, not `update`.
        rule(
            &["coordination.k8s.io"],
            &["leases"],
            &["get", "create", "patch"],
        ),
        // Events: lifecycle Events on Sandboxes. The kube Recorder creates and
        // patches Events.
        rule(&[""], &["events"], &["create", "patch"]),
    ]
}

fn generate_rbac() -> Result<()> {
    let role = ClusterRole {
        metadata: ObjectMeta {
            name: Some("msb-controller".to_string()),
            ..Default::default()
        },
        rules: Some(controller_policy_rules()),
        ..Default::default()
    };

    let yaml = serde_yaml::to_string(&role).context("serialize ClusterRole to YAML")?;

    let out = Path::new("deploy/rbac/controller-clusterrole.yaml");
    fs::create_dir_all(out.parent().unwrap()).context("create deploy/rbac")?;
    fs::write(out, &yaml).context("write controller-clusterrole.yaml")?;

    println!("wrote {}", out.display());
    Ok(())
}

fn generate_crd() -> Result<()> {
    let mut crd = Sandbox::crd();

    // Seal every spec field except the mutable allowlist. kube-derive can't emit CEL
    // transition rules, so inject a `self == oldSelf` rule per field. `oldSelf` binds
    // only on update, so these seal after creation and are skipped on create.
    const MUTABLE_FIELDS: &[&str] = &["desiredState", "cpus", "memory", "lifecycle"];

    for version in &mut crd.spec.versions {
        if version.name != "v1alpha1" {
            continue;
        }
        let Some(schema) = version.schema.as_mut() else {
            continue;
        };
        let Some(root) = schema.open_api_v3_schema.as_mut() else {
            continue;
        };
        let Some(spec) = root.properties.as_mut().and_then(|p| p.get_mut("spec")) else {
            continue;
        };
        spec.x_kubernetes_validations
            .get_or_insert_with(Vec::new)
            .extend([
                ValidationRule {
                    rule: "self.cpus <= (has(self.maxCpus) ? self.maxCpus : self.cpus)".to_string(),
                    message: Some("spec.cpus must not exceed spec.maxCpus".to_string()),
                    ..Default::default()
                },
                ValidationRule {
                    rule: "self.memory <= (has(self.maxMemory) ? self.maxMemory : self.memory)"
                        .to_string(),
                    message: Some("spec.memory must not exceed spec.maxMemory".to_string()),
                    ..Default::default()
                },
            ]);

        let Some(fields) = spec.properties.as_mut() else {
            continue;
        };
        for (name, field) in fields.iter_mut() {
            if MUTABLE_FIELDS.contains(&name.as_str()) {
                continue;
            }
            field
                .x_kubernetes_validations
                .get_or_insert_with(Vec::new)
                .push(ValidationRule {
                    rule: "self == oldSelf".to_string(),
                    message: Some(format!("spec.{name} is immutable after creation")),
                    ..Default::default()
                });
        }

        // `status.conditions` is a standard `metav1.Condition` list. schemars can't
        // emit the list-map markers, so stamp them here: server-side apply then
        // merges conditions by `type` instead of replacing the whole array.
        if let Some(conditions) = root
            .properties
            .as_mut()
            .and_then(|p| p.get_mut("status"))
            .and_then(|s| s.properties.as_mut())
            .and_then(|p| p.get_mut("conditions"))
        {
            conditions.x_kubernetes_list_type = Some("map".to_string());
            conditions.x_kubernetes_list_map_keys = Some(vec!["type".to_string()]);
        }
    }

    let yaml = serde_yaml::to_string(&crd).context("serialize CRD to YAML")?;

    let out = Path::new("deploy/crds/sandboxes.yaml");
    fs::create_dir_all(out.parent().unwrap()).context("create deploy/crds")?;
    fs::write(out, &yaml).context("write sandboxes.yaml")?;

    println!("wrote {}", out.display());
    Ok(())
}
