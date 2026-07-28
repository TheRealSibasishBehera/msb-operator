use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use futures::TryFutureExt;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::watch;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::pb::{
    AllocateRequest, AllocateResponse, ContainerAllocateResponse, Device, DevicePluginOptions,
    DeviceSpec, Empty, ListAndWatchResponse, Mount, PreStartContainerRequest,
    PreStartContainerResponse, PreferredAllocationRequest, PreferredAllocationResponse,
    RegisterRequest,
    device_plugin_server::{DevicePlugin, DevicePluginServer},
    registration_client::RegistrationClient,
};

const RESOURCE_NAME: &str = "devices.microsandbox.io/kvm";

/// Must be `/msb/cache` so the VMDK's baked absolute extents resolve.
const CACHE_MOUNT: &str = "/msb/cache";
const API_VERSION: &str = "v1beta1";
const KUBELET_SOCKET: &str = "/var/lib/kubelet/device-plugins/kubelet.sock";
const PLUGIN_SOCKET_NAME: &str = "msb-kvm.sock";
const PLUGIN_DIR: &str = "/var/lib/kubelet/device-plugins";

const HEALTHY: &str = "Healthy";
const UNHEALTHY: &str = "Unhealthy";

const POOL_SIZE: usize = 1000;

fn build_device_list(healthy: bool) -> Vec<Device> {
    let health = if healthy { HEALTHY } else { UNHEALTHY }.to_owned();
    (0..POOL_SIZE)
        .map(|i| Device {
            id: format!("kvm-{i}"),
            health: health.clone(),
            topology: None,
        })
        .collect()
}

pub struct KvmDevicePlugin {
    health_rx: watch::Receiver<bool>,
    /// Host cache dir, injected read-only into each sandbox at `CACHE_MOUNT`.
    cache_host_path: PathBuf,
}

impl KvmDevicePlugin {
    pub fn new(health_rx: watch::Receiver<bool>, cache_host_path: PathBuf) -> Self {
        Self {
            health_rx,
            cache_host_path,
        }
    }
}

#[tonic::async_trait]
impl DevicePlugin for KvmDevicePlugin {
    async fn get_device_plugin_options(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<DevicePluginOptions>, Status> {
        Ok(Response::new(DevicePluginOptions {
            pre_start_required: false,
            get_preferred_allocation_available: false,
        }))
    }

    type ListAndWatchStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<ListAndWatchResponse, Status>> + Send>,
    >;

    async fn list_and_watch(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::ListAndWatchStream>, Status> {
        let health_rx = self.health_rx.clone();
        let (resp_tx, resp_rx) = watch::channel(ListAndWatchResponse {
            devices: build_device_list(*health_rx.borrow()),
        });

        tokio::spawn(async move {
            let mut health = health_rx;
            loop {
                if health.changed().await.is_err() {
                    break;
                }
                let healthy = *health.borrow();
                info!(healthy, "sending updated device list to kubelet");
                if resp_tx
                    .send(ListAndWatchResponse {
                        devices: build_device_list(healthy),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let stream = tokio_stream::wrappers::WatchStream::new(resp_rx).map(Ok);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_preferred_allocation(
        &self,
        _request: Request<PreferredAllocationRequest>,
    ) -> Result<Response<PreferredAllocationResponse>, Status> {
        Ok(Response::new(PreferredAllocationResponse {
            container_responses: vec![],
        }))
    }

    async fn allocate(
        &self,
        request: Request<AllocateRequest>,
    ) -> Result<Response<AllocateResponse>, Status> {
        // A node path (the DaemonSet's DirectoryOrCreate hostPath makes it), not
        // creatable from this container — just the string the kubelet mounts.
        let cache_host_path = self.cache_host_path.display().to_string();

        // Deliver the cache as a device-plugin Mount, not a pod volume: injected
        // kubelet-side, it never appears in the pod spec, so PSA stays `restricted`.
        let responses = request
            .into_inner()
            .container_requests
            .into_iter()
            .map(|_| ContainerAllocateResponse {
                devices: vec![DeviceSpec {
                    host_path: "/dev/kvm".to_owned(),
                    container_path: "/dev/kvm".to_owned(),
                    permissions: "rw".to_owned(),
                }],
                envs: Default::default(),
                mounts: vec![Mount {
                    host_path: cache_host_path.clone(),
                    container_path: CACHE_MOUNT.to_owned(),
                    read_only: true,
                }],
                annotations: Default::default(),
                cdi_devices: vec![],
            })
            .collect();

        Ok(Response::new(AllocateResponse {
            container_responses: responses,
        }))
    }

    async fn pre_start_container(
        &self,
        _request: Request<PreStartContainerRequest>,
    ) -> Result<Response<PreStartContainerResponse>, Status> {
        Ok(Response::new(PreStartContainerResponse {}))
    }
}

async fn register(socket_name: &str) -> anyhow::Result<()> {
    let channel = tonic::transport::Endpoint::try_from("http://localhost")?
        .connect_with_connector(tower::service_fn({
            let path = KUBELET_SOCKET.to_owned();
            move |_: tonic::transport::Uri| {
                tokio::net::UnixStream::connect(path.clone()).map_ok(hyper_util::rt::TokioIo::new)
            }
        }))
        .await
        .context("connecting to kubelet registration socket")?;

    let mut client = RegistrationClient::new(channel);
    client
        .register(RegisterRequest {
            version: API_VERSION.to_owned(),
            endpoint: socket_name.to_owned(),
            resource_name: RESOURCE_NAME.to_owned(),
            options: Some(DevicePluginOptions {
                pre_start_required: false,
                get_preferred_allocation_available: false,
            }),
        })
        .await
        .context("Register RPC failed")?;

    info!("registered {RESOURCE_NAME} with kubelet");
    Ok(())
}

/// Resolves when kubelet.sock is recreated, which is the authoritative signal
/// that the kubelet restarted and expects fresh plugin re-registration.
async fn wait_for_kubelet_restart(kubelet_sock: &Path) -> anyhow::Result<()> {
    let target = kubelet_sock.to_owned();
    let parent = kubelet_sock
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .to_owned();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);

    let mut watcher = RecommendedWatcher::new(
        move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result {
                let _ = event_tx.blocking_send(event);
            }
        },
        notify::Config::default(),
    )?;
    watcher.watch(&parent, RecursiveMode::NonRecursive)?;

    while let Some(event) = event_rx.recv().await {
        let created = matches!(event.kind, EventKind::Create(_));
        if created && event.paths.iter().any(|p| p == &target) {
            return Ok(());
        }
    }

    Ok(())
}

pub async fn run(
    health_rx: watch::Receiver<bool>,
    cache_host_path: PathBuf,
) -> anyhow::Result<()> {
    loop {
        let socket_path = PathBuf::from(PLUGIN_DIR).join(PLUGIN_SOCKET_NAME);

        let _ = std::fs::remove_file(&socket_path);

        let plugin = KvmDevicePlugin::new(health_rx.clone(), cache_host_path.clone());
        let server = DevicePluginServer::new(plugin);

        let listener = {
            use tokio::net::UnixListener;
            UnixListener::bind(&socket_path)
                .with_context(|| format!("binding {}", socket_path.display()))?
        };

        // Spawn the server before registering: kubelet dials ListAndWatch
        // immediately after Register returns, so connections must be accepted.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let serve = {
            let server = server;
            let incoming = async_stream::stream! {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => yield Ok::<_, std::io::Error>(stream),
                        Err(e) => warn!("accept error: {e}"),
                    }
                }
            };
            tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(server)
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = shutdown_rx.await;
                    }),
            )
        };

        register(PLUGIN_SOCKET_NAME)
            .await
            .context("registration with kubelet")?;

        info!("device plugin serving on {}", socket_path.display());

        wait_for_kubelet_restart(Path::new(KUBELET_SOCKET)).await?;
        info!("kubelet restarted — re-registering in 5s");

        let _ = shutdown_tx.send(());
        let _ = serve.await;

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_pool_has_correct_size_and_ids() {
        let list = build_device_list(true);
        assert_eq!(list.len(), POOL_SIZE);
        assert_eq!(list[0].id, "kvm-0");
        assert_eq!(list[POOL_SIZE - 1].id, format!("kvm-{}", POOL_SIZE - 1));
        assert!(list.iter().all(|d| d.health == HEALTHY));
    }

    #[test]
    fn unhealthy_pool_marks_all_devices() {
        let list = build_device_list(false);
        assert!(list.iter().all(|d| d.health == UNHEALTHY));
    }

    #[test]
    fn pool_ids_are_unique() {
        let list = build_device_list(true);
        let ids: std::collections::HashSet<_> = list.iter().map(|d| &d.id).collect();
        assert_eq!(ids.len(), POOL_SIZE);
    }

    #[tokio::test]
    async fn allocate_injects_kvm_and_a_read_only_cache_mount() {
        use crate::pb::ContainerAllocateRequest;

        let cache = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(true);
        let plugin = KvmDevicePlugin::new(rx, cache.path().to_path_buf());

        let resp = plugin
            .allocate(Request::new(AllocateRequest {
                container_requests: vec![ContainerAllocateRequest {
                    devices_ids: vec!["kvm-0".to_owned()],
                }],
            }))
            .await
            .unwrap()
            .into_inner();

        let cr = &resp.container_responses[0];
        assert_eq!(cr.devices[0].container_path, "/dev/kvm");

        assert_eq!(cr.mounts.len(), 1);
        assert_eq!(cr.mounts[0].container_path, "/msb/cache");
        assert_eq!(cr.mounts[0].host_path, cache.path().display().to_string());
        assert!(cr.mounts[0].read_only);
    }
}
