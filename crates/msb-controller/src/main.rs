mod conditions;
mod config;
mod controller;
mod leader;
mod pod;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::controller::Controller;
use kube::runtime::events::{Recorder, Reporter};
use kube::runtime::watcher;
use kube::{Api, Client};
use msb_crd::Sandbox;
use tracing::{info, warn};

use crate::config::ControllerConfig;
use crate::controller::Context;

#[derive(Parser)]
#[command(name = "msb-controller", about = "microsandbox Sandbox controller")]
struct Cli {
    /// msb home inside sandbox pods. Must be `/msb`: the cache's baked VMDK
    /// holds absolute paths under `/msb/cache`, so build and runtime must match.
    #[arg(long, default_value = "/msb", env = "MSB_HOME")]
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

    /// Namespace where the controller runs and holds its leader-election Lease.
    #[arg(long, default_value = "msb-system", env = "MSB_NAMESPACE")]
    namespace: String,

    /// Leader-election lease lifetime. A standby acquires after this elapses
    /// without a renewal from the current leader.
    #[arg(long, default_value = "15", env = "MSB_LEASE_TTL_SECS")]
    lease_ttl_secs: u64,
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

    // Unique per replica: pod hostname distinguishes holders; pid disambiguates
    // if two ever share a hostname (e.g. host-network).
    let holder_id = format!(
        "{}-{}",
        hostname::get()?.to_string_lossy(),
        std::process::id()
    );

    info!(%config, holder = %holder_id, "starting controller");

    let client_for_lease = client.clone();
    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: "msb-controller".into(),
            instance: Some(holder_id.clone()),
        },
    );
    let run_controller = || async move {
        Controller::new(sandboxes, watcher::Config::default())
            .owns(pods, watcher::Config::default())
            .shutdown_on_signal()
            .run(
                controller::reconcile,
                controller::error_policy,
                Arc::new(Context {
                    client,
                    config,
                    recorder,
                }),
            )
            .for_each(|res| async move {
                match res {
                    Ok((obj, _)) => info!(sandbox = %obj.name, "reconciled"),
                    Err(error) => warn!(%error, "reconcile error"),
                }
            })
            .await;
    };

    leader::run_when_leader(
        client_for_lease,
        &cli.namespace,
        &holder_id,
        Duration::from_secs(cli.lease_ttl_secs),
        run_controller,
    )
    .await?;

    Ok(())
}
