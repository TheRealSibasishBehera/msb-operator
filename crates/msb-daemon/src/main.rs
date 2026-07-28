mod device_plugin;
mod health;
mod marker;
mod pull;
mod watch;

pub(crate) mod pb {
    tonic::include_proto!("v1beta1");
}

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing::info;

#[derive(Parser)]
#[command(name = "msb-daemon", about = "microsandbox per-node daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    DevicePlugin(DevicePluginArgs),
    Pull(PullArgs),
}

#[derive(Parser)]
struct DevicePluginArgs {
    /// Path to the KVM device node to watch.
    #[arg(long, default_value = "/dev/kvm")]
    kvm_path: PathBuf,

    /// Host cache dir, injected read-only into each sandbox alongside /dev/kvm.
    #[arg(long, default_value = "/var/lib/msb/cache", env = "MSB_CACHE_HOST_PATH")]
    cache_host_path: PathBuf,

    /// This node's name (downward API), for the node-scoped sandbox-pod watch.
    #[arg(long, env = "MSB_NODE_NAME")]
    node_name: String,

    /// `MSB_HOME` for pulls. Fixed at /msb so the VMDK bakes /msb/cache extents;
    /// the daemon mounts the host cache dir there so writes land in the cache.
    #[arg(long, default_value = "/msb", env = "MSB_HOME")]
    msb_home: PathBuf,

    /// Path to the `msb` binary.
    #[arg(long, default_value = "/usr/local/bin/msb", env = "MSB_PATH")]
    msb_path: PathBuf,
}

#[derive(Parser)]
struct PullArgs {
    /// OCI image reference to pull and convert (e.g. `python:3.12`).
    image: String,

    /// `MSB_HOME` for the pull; the cache lands under `<msb-home>/cache`.
    #[arg(long, default_value = "/msb", env = "MSB_HOME")]
    msb_home: PathBuf,

    /// Path to the `msb` binary.
    #[arg(long, default_value = "/usr/local/bin/msb", env = "MSB_PATH")]
    msb_path: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::DevicePlugin(args) => {
            info!(kvm = %args.kvm_path.display(), node = %args.node_name, "starting daemon");
            let health_rx = health::watch_path(args.kvm_path)
                .await
                .context("initialising health watcher")?;
            // Device plugin and pod watch run for the process's life; either
            // returning (only ever as an error) brings the daemon down.
            tokio::try_join!(
                device_plugin::run(health_rx, args.cache_host_path),
                watch::run(args.node_name, args.msb_path, args.msb_home),
            )?;
            Ok(())
        }
        Commands::Pull(args) => {
            pull::pull(&args.msb_path, &args.msb_home, &args.image)
                .await
                .context("pulling image")
        }
    }
}
