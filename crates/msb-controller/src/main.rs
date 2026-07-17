mod config;
mod controller;
mod pod;

use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::controller::Controller;
use kube::runtime::watcher;
use kube::{Api, Client};
use msb_crd::Sandbox;
use tracing::{info, warn};

use crate::config::ControllerConfig;
use crate::controller::Context;

#[derive(Parser)]
#[command(name = "msb-controller", about = "microsandbox Sandbox controller")]
struct Cli {
    /// msb state directory on the node, mounted as a hostPath into sandbox pods.
    #[arg(long, default_value = "/var/lib/msb", env = "MSB_HOME")]
    msb_home: String,

    /// GID of /dev/kvm on the node. Not standardised; Ubuntu assigns it dynamically.
    #[arg(long, env = "MSB_KVM_GID")]
    kvm_gid: i64,

    #[arg(long, env = "MSB_PRERUNNER_IMAGE")]
    prerunner_image: String,

    #[arg(long, env = "MSB_RUNTIME_IMAGE")]
    runtime_image: String,

    #[arg(long, env = "MSB_BRIDGE_IMAGE")]
    bridge_image: String,

    #[arg(long, default_value_t = 7000, env = "MSB_BRIDGE_PORT")]
    bridge_port: i32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let config = ControllerConfig::new(
        cli.msb_home,
        cli.kvm_gid,
        cli.prerunner_image,
        cli.runtime_image,
        cli.bridge_image,
        cli.bridge_port,
    )
    .context("invalid controller configuration")?;

    let client = Client::try_default()
        .await
        .context("connecting to the Kubernetes API")?;

    let sandboxes: Api<Sandbox> = Api::all(client.clone());
    let pods: Api<Pod> = Api::all(client.clone());

    // Fail fast with a usable message rather than looping on watch errors.
    sandboxes
        .list(&Default::default())
        .await
        .context("listing Sandboxes; is the CRD applied?")?;

    info!(%config, "starting controller");

    Controller::new(sandboxes, watcher::Config::default())
        .owns(pods, watcher::Config::default())
        .shutdown_on_signal()
        .run(
            controller::reconcile,
            controller::error_policy,
            Arc::new(Context { client, config }),
        )
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => info!(sandbox = %obj.name, "reconciled"),
                Err(error) => warn!(%error, "reconcile error"),
            }
        })
        .await;

    Ok(())
}
