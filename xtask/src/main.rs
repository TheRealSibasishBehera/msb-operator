use anyhow::{Context, Result, bail};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::ValidationRule;
use kube::CustomResourceExt;
use msb_crd::Sandbox;
use std::{fs, path::Path};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("generate-crd") => generate_crd(),
        Some(cmd) => bail!("unknown command: {cmd}"),
        None => bail!(
            "usage: cargo xtask <command>\n  generate-crd    write deploy/crds/sandboxes.yaml"
        ),
    }
}

fn generate_crd() -> Result<()> {
    let mut crd = Sandbox::crd();

    // kube-derive 0.99 does not support CEL rules via the derive macro.
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
