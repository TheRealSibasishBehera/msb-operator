use anyhow::{Context, Result, bail};
use k8s_openapi::api::rbac::v1::{ClusterRole, PolicyRule};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::ValidationRule;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::CustomResourceExt;
use msb_crd::Sandbox;
use std::{fs, path::Path};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("generate-crd") => generate_crd(),
        Some("generate-rbac") => generate_rbac(),
        Some(cmd) => bail!("unknown command: {cmd}"),
        None => bail!(
            "usage: cargo xtask <command>\n  \
             generate-crd     write deploy/crds/sandboxes.yaml\n  \
             generate-rbac    write deploy/rbac/controller-clusterrole.yaml"
        ),
    }
}

/// The controller ServiceAccount's cluster-wide permissions.
///
/// This list is the source of truth for controller RBAC. Each rule maps to a
/// concrete `Api` call in `msb-controller`; review the two together. Nothing
/// grants Secret access — resolving secrets is the prerunner's job under a
/// namespace-scoped Role (provisioned separately).
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
            &["sandbox.microsandbox.io"],
            &["sandboxes"],
            &["get", "list", "watch", "delete"],
        ),
        // Status subresource: only ever patch_status'd, never GET separately.
        rule(
            &["sandbox.microsandbox.io"],
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
        // Leases: leader election. kube-leader-election creates the Lease once
        // then renews/acquires/steps-down via server-side apply (PATCH), so
        // `patch` is required, not `update`.
        rule(
            &["coordination.k8s.io"],
            &["leases"],
            &["get", "create", "patch"],
        ),
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

    // kube-derive does not support CEL transition rules via the derive macro.
    let cel_rule = ValidationRule {
        rule: "self.spec == oldSelf.spec".to_string(),
        message: Some("Sandbox spec is immutable after creation".to_string()),
        ..Default::default()
    };

    for version in &mut crd.spec.versions {
        if version.name != "v1alpha1" {
            continue;
        }
        if let Some(schema) = version.schema.as_mut() {
            if let Some(props) = schema.open_api_v3_schema.as_mut() {
                props
                    .x_kubernetes_validations
                    .get_or_insert_with(Vec::new)
                    .push(cel_rule.clone());
            }
        }
    }

    let yaml = serde_yaml::to_string(&crd).context("serialize CRD to YAML")?;

    let out = Path::new("deploy/crds/sandboxes.yaml");
    fs::create_dir_all(out.parent().unwrap()).context("create deploy/crds")?;
    fs::write(out, &yaml).context("write sandboxes.yaml")?;

    println!("wrote {}", out.display());
    Ok(())
}
