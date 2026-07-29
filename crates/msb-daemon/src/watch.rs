//! Pulls each sandbox pod's image into the node cache as it schedules here.
//!
//! Pods, not Sandboxes: `spec.nodeName` is field-selectable, so the watch
//! narrows to this node server-side.

use std::path::PathBuf;

use anyhow::Context;
use futures::TryStreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::watcher::{self, Event};
use kube::{Api, Client};
use tracing::{info, warn};

use crate::{marker, pull};

const SANDBOX_LABEL: &str = "microsandbox.dev/sandbox";
const RUNTIME_CONTAINER: &str = "msb-runtime";
const SPEC_ENV: &str = "MSB_SANDBOX_SPEC";

pub async fn run(node_name: String, msb: PathBuf, msb_home: PathBuf) -> anyhow::Result<()> {
    let client = Client::try_default()
        .await
        .context("connecting to the Kubernetes API")?;
    let pods: Api<Pod> = Api::all(client);

    let config = watcher::Config::default()
        .labels(&format!("{SANDBOX_LABEL}=true"))
        .fields(&format!("spec.nodeName={node_name}"));

    info!(node = %node_name, "watching sandbox pods on this node");

    watcher::watcher(pods, config)
        .try_for_each(|event| {
            let (msb, msb_home) = (msb.clone(), msb_home.clone());
            async move {
                // Apply = add or restart-relist; the pull is idempotent either way.
                if let Event::Apply(pod) = event {
                    handle_pod(&pod, &msb, &msb_home).await;
                }
                Ok(())
            }
        })
        .await
        .context("watching sandbox pods")?;

    Ok(())
}

async fn handle_pod(pod: &Pod, msb: &std::path::Path, msb_home: &std::path::Path) {
    let name = pod.metadata.name.as_deref().unwrap_or("<unknown>");
    let Some(image) = pod_image(pod) else {
        warn!(pod = %name, "sandbox pod has no {SPEC_ENV} image; skipping");
        return;
    };

    let cache_root = msb_home.join("cache");
    match pull::pull(msb, msb_home, &image).await {
        Ok(()) => marker::write_ready(&cache_root, &image),
        Err(e) => {
            warn!(pod = %name, %image, error = %e, "pull failed");
            marker::write_failed(&cache_root, &image, &e.to_string());
        }
    }
}

/// The sandbox image, read from the runtime container's `MSB_SANDBOX_SPEC` env.
fn pod_image(pod: &Pod) -> Option<String> {
    let spec_json = pod
        .spec
        .as_ref()?
        .containers
        .iter()
        .find(|c| c.name == RUNTIME_CONTAINER)?
        .env
        .as_ref()?
        .iter()
        .find(|e| e.name == SPEC_ENV)?
        .value
        .as_ref()?;
    let spec: serde_json::Value = serde_json::from_str(spec_json).ok()?;
    spec.get("image")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{Container, EnvVar, PodSpec};

    fn pod_with_runtime_env(name: &str, value: Option<&str>) -> Pod {
        Pod {
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: name.to_owned(),
                    env: value.map(|v| {
                        vec![EnvVar {
                            name: SPEC_ENV.to_owned(),
                            value: Some(v.to_owned()),
                            ..Default::default()
                        }]
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn reads_image_from_the_runtime_spec_env() {
        let pod = pod_with_runtime_env(RUNTIME_CONTAINER, Some(r#"{"image":"python:3.12"}"#));
        assert_eq!(pod_image(&pod).as_deref(), Some("python:3.12"));
    }

    #[test]
    fn no_image_when_spec_env_is_absent() {
        let pod = pod_with_runtime_env(RUNTIME_CONTAINER, None);
        assert_eq!(pod_image(&pod), None);
    }

    #[test]
    fn no_image_when_container_is_not_the_runtime() {
        let pod = pod_with_runtime_env("msb-bridge", Some(r#"{"image":"python:3.12"}"#));
        assert_eq!(pod_image(&pod), None);
    }
}
