use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EmptyDirVolumeSource, EnvVar, KeyToPath, Pod,
    PodSecurityContext, PodSpec, ResourceRequirements, SeccompProfile, SecretVolumeSource,
    SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Resource;
use msb_crd::{Sandbox, SandboxSpec};

use crate::config::{
    CPU_ALLOCATION_RATIO, ControllerConfig, EPHEMERAL_STORAGE_MIB, KVM_RESOURCE, SANDBOX_LABEL,
    memory_overhead_mib,
};

#[derive(Debug, thiserror::Error)]
pub enum PodBuildError {
    #[error("sandbox {sandbox} is missing .metadata.{key}")]
    MissingObjectKey { sandbox: String, key: &'static str },

    #[error("serialising spec for sandbox {sandbox}: {source}")]
    SpecEncode {
        sandbox: String,
        #[source]
        source: serde_json::Error,
    },
}

const VOL_HOME: &str = "msb-home";

/// Referenced Secrets mount read-only here, one dir per Secret; the runtime reads
/// `<SECRETS_MOUNT>/<secretName>/<key>`. The kubelet does the read, so the pod
/// needs no Secret RBAC.
const SECRETS_MOUNT: &str = "/msb-secrets";

/// Sandbox pods run as this fixed non-root uid; `/root/.microsandbox` (msb's
/// default home) is unreadable to it, which is why MSB_HOME is set explicitly.
const RUN_AS_USER: i64 = 1000;

pub fn pod_name(sandbox_name: &str) -> String {
    format!("sandbox-{sandbox_name}")
}

/// msb's flat sandbox name: `<namespace>__<name>`. `_` is forbidden in
/// Kubernetes names but permitted by msb, so `__` is an unambiguous separator.
fn msb_sandbox_name(namespace: &str, name: &str) -> String {
    format!("{namespace}__{name}")
}

/// Builds the sandbox Pod. Pure: no client, no I/O.
pub fn build(sandbox: &Sandbox, cfg: &ControllerConfig) -> Result<Pod, PodBuildError> {
    let name = sandbox
        .meta()
        .name
        .clone()
        .ok_or_else(|| PodBuildError::MissingObjectKey {
            sandbox: "<unnamed>".to_string(),
            key: "name",
        })?;
    let namespace =
        sandbox
            .meta()
            .namespace
            .clone()
            .ok_or_else(|| PodBuildError::MissingObjectKey {
                sandbox: name.clone(),
                key: "namespace",
            })?;

    // Returns None when .metadata.uid is unset, which only happens for objects
    // that never came from the API server.
    let owner =
        sandbox
            .controller_owner_ref(&())
            .ok_or_else(|| PodBuildError::MissingObjectKey {
                sandbox: name.clone(),
                key: "uid",
            })?;

    let spec_json =
        serde_json::to_string(&sandbox.spec).map_err(|source| PodBuildError::SpecEncode {
            sandbox: name.clone(),
            source,
        })?;

    let flat_name = msb_sandbox_name(&namespace, &name);

    let labels = BTreeMap::from([
        (SANDBOX_LABEL.to_string(), "true".to_string()),
        ("microsandbox.dev/sandbox-name".to_string(), name.clone()),
    ]);

    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(pod_name(&name)),
            namespace: Some(namespace.clone()),
            labels: Some(labels),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            // The bridge is a native sidecar, so it lives in init_containers but
            // stays up for the pod's life and is ready in parallel with the boot.
            init_containers: Some(vec![bridge_container(cfg, &flat_name)]),
            containers: vec![runtime_container(
                cfg,
                &spec_json,
                &flat_name,
                &sandbox.spec,
            )],
            volumes: Some(volumes(&sandbox.spec)),
            security_context: Some(PodSecurityContext {
                run_as_non_root: Some(true),
                run_as_user: Some(RUN_AS_USER),
                // The device plugin handles the cgroup allowlist but not Unix DAC;
                // /dev/kvm is crw-rw---- root:kvm, so the process must be in the group.
                supplemental_groups: Some(vec![cfg.kvm_gid]),
                // The KVM ioctls the runtime needs are not blocked by the
                // runtime's default seccomp profile.
                seccomp_profile: Some(SeccompProfile {
                    type_: "RuntimeDefault".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        status: None,
    })
}

fn runtime_container(
    cfg: &ControllerConfig,
    spec_json: &str,
    flat_name: &str,
    spec: &SandboxSpec,
) -> Container {
    // No cache mount: the daemon's device plugin injects it read-only at
    // $MSB_HOME/cache, keeping the pod volume-free and PSA `restricted`-clean.
    let mut mounts = vec![VolumeMount {
        name: VOL_HOME.to_string(),
        mount_path: cfg.msb_home().to_string(),
        ..Default::default()
    }];
    // The runtime resolves secrets in-process from these mounts — no init container.
    for secret_name in referenced_secret_names(spec) {
        mounts.push(VolumeMount {
            name: secret_vol_name(&secret_name),
            mount_path: format!("{SECRETS_MOUNT}/{secret_name}"),
            read_only: Some(true),
            ..Default::default()
        });
    }

    Container {
        name: "msb-runtime".to_string(),
        image: Some(cfg.runtime_image.clone()),
        // msb and libkrunfw are baked into the image, so only the spec and flat
        // name are passed, not binary paths.
        env: Some(vec![
            EnvVar {
                name: "MSB_SANDBOX_SPEC".to_string(),
                value: Some(spec_json.to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "MSB_SANDBOX_NAME".to_string(),
                value: Some(flat_name.to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "MSB_HOME".to_string(),
                value: Some(cfg.msb_home().to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "RUST_LOG".to_string(),
                value: Some(cfg.runtime_log.clone()),
                ..Default::default()
            },
        ]),
        volume_mounts: Some(mounts),
        resources: Some(runtime_resources(spec.cpus, spec.memory)),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            capabilities: Some(Capabilities {
                // msb's network is smoltcp — a userspace TCP/IP stack in-process
                // (no tap/tun/vhost) — so the VMM needs no capabilities.
                drop: Some(vec!["ALL".to_string()]),
                add: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Resources for the runtime container. Memory request == limit (guest RAM +
/// overhead): the guest has a hard `--memory-mib` ceiling, so a matching limit
/// is safe and keeps a busy sandbox from OOMing its neighbours. CPU requests a
/// fraction of a core and limits to the advertised vCPUs.
fn runtime_resources(cpus: u32, memory_mib: u32) -> ResourceRequirements {
    let mem_total = memory_mib as u64 + memory_overhead_mib(memory_mib as u64, cpus as u64);
    let mem_qty = Quantity(format!("{mem_total}Mi"));
    let cpu_request = Quantity(format!(
        "{}m",
        (cpus as u64 * 1000).div_ceil(CPU_ALLOCATION_RATIO)
    ));
    let cpu_limit = Quantity(cpus.to_string());
    let ephemeral = Quantity(format!("{EPHEMERAL_STORAGE_MIB}Mi"));

    let requests = BTreeMap::from([
        ("memory".to_string(), mem_qty.clone()),
        ("cpu".to_string(), cpu_request),
        ("ephemeral-storage".to_string(), ephemeral),
    ]);
    let limits = BTreeMap::from([
        ("memory".to_string(), mem_qty),
        ("cpu".to_string(), cpu_limit),
        (KVM_RESOURCE.to_string(), Quantity("1".to_string())),
    ]);

    ResourceRequirements {
        requests: Some(requests),
        limits: Some(limits),
        ..Default::default()
    }
}

fn bridge_container(cfg: &ControllerConfig, flat_name: &str) -> Container {
    Container {
        name: "msb-bridge".to_string(),
        image: Some(cfg.bridge_image.clone()),
        // restartPolicy: Always on an init container makes it a native sidecar.
        restart_policy: Some("Always".to_string()),
        // The bridge locates the relay socket under $MSB_HOME/run from the name.
        env: Some(vec![
            EnvVar {
                name: "MSB_SANDBOX_NAME".to_string(),
                value: Some(flat_name.to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "MSB_HOME".to_string(),
                value: Some(cfg.msb_home().to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "MSB_BRIDGE_PORT".to_string(),
                value: Some(cfg.bridge_port.to_string()),
                ..Default::default()
            },
        ]),
        ports: Some(vec![ContainerPort {
            name: Some("agent".to_string()),
            container_port: cfg.bridge_port,
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }]),
        // The agent socket lives under $MSB_HOME/run, which msb cannot relocate,
        // so the bridge reaches it through the shared home rather than its own volume.
        volume_mounts: Some(vec![VolumeMount {
            name: VOL_HOME.to_string(),
            mount_path: cfg.msb_home().to_string(),
            read_only: Some(true),
            ..Default::default()
        }]),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            // Writes only to its mounted volumes, never the container rootfs.
            read_only_root_filesystem: Some(true),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                add: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The distinct Secret names referenced by `spec.secrets`, in stable order.
fn referenced_secret_names(spec: &SandboxSpec) -> Vec<String> {
    let mut names: Vec<String> = spec
        .secrets
        .iter()
        .map(|s| s.value_from.secret_key_ref.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Volume name for a referenced Secret. Sanitized to a DNS-1123 label since
/// Secret names permit `.`, which volume names forbid.
fn secret_vol_name(secret_name: &str) -> String {
    let sanitized: String = secret_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("secret-{}", sanitized.trim_matches('-'))
}

fn volumes(spec: &SandboxSpec) -> Vec<Volume> {
    // Per-pod writable home (db/, sandboxes/, run/): keeps each sandbox's state
    // off the node and invisible to other pods. The shared read-only cache is not
    // a volume — the daemon's device plugin injects it (see runtime_container).
    let mut vols = vec![Volume {
        name: VOL_HOME.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    }];

    // One volume per referenced Secret, projecting only the referenced keys.
    for secret_name in referenced_secret_names(spec) {
        let mut keys: Vec<String> = spec
            .secrets
            .iter()
            .filter(|s| s.value_from.secret_key_ref.name == secret_name)
            .map(|s| s.value_from.secret_key_ref.key.clone())
            .collect();
        keys.sort();
        keys.dedup();
        let items = keys
            .into_iter()
            .map(|key| KeyToPath {
                path: key.clone(),
                key,
                ..Default::default()
            })
            .collect();
        vols.push(Volume {
            name: secret_vol_name(&secret_name),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret_name),
                items: Some(items),
                // Secret files are root-owned, so owner-only mode would deny the
                // non-root uid the pod runs as. tmpfs, per-pod.
                default_mode: Some(0o444),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    vols
}

#[cfg(test)]
pub(crate) mod test_support {
    use kube::api::ObjectMeta;
    use msb_crd::{Sandbox, SandboxSpec};

    use crate::config::ControllerConfig;

    pub fn sandbox() -> Sandbox {
        Sandbox {
            metadata: ObjectMeta {
                name: Some("my-sandbox".to_string()),
                namespace: Some("team-a".to_string()),
                uid: Some("d1e2f3a4-0000-0000-0000-000000000000".to_string()),
                ..Default::default()
            },
            spec: SandboxSpec {
                image: "python:3.12".to_string(),
                cpus: 2,
                memory: 1024,
                cmd: vec!["python".to_string(), "script.py".to_string()],
                ephemeral: true,
                run_policy: Default::default(),
                secrets: Vec::new(),
                network: Default::default(),
                upper: Default::default(),
                volumes: Vec::new(),
            },
            status: None,
        }
    }

    /// A sandbox with two secrets: two keys from `creds`, one from `db`.
    pub fn sandbox_with_secrets() -> Sandbox {
        use msb_crd::sandbox::{SecretEntry, SecretKeyRef, SecretValueFrom};
        let entry = |env: &str, name: &str, key: &str| SecretEntry {
            env: env.to_string(),
            value_from: SecretValueFrom {
                secret_key_ref: SecretKeyRef {
                    name: name.to_string(),
                    key: key.to_string(),
                },
            },
            allowed_hosts: vec!["api.example.com".to_string()],
        };
        let mut sb = sandbox();
        sb.spec.secrets = vec![
            entry("API_KEY", "creds", "api"),
            entry("ORG_ID", "creds", "org"),
            entry("DB_PASS", "db", "password"),
        ];
        sb
    }

    pub fn config() -> ControllerConfig {
        ControllerConfig::new(
            // MSB_HOME must be /msb so the cache's absolute VMDK paths resolve.
            "/msb",
            104,
            "ghcr.io/msb/runtime:dev",
            "ghcr.io/msb/bridge:dev",
            7000,
        )
        .expect("valid test config")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{config, sandbox, sandbox_with_secrets};
    use super::*;

    fn container<'a>(pod: &'a Pod, name: &str) -> &'a Container {
        pod.spec
            .as_ref()
            .unwrap()
            .containers
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no container named {name}"))
    }

    fn container_or_init<'a>(pod: &'a Pod, name: &str) -> &'a Container {
        let spec = pod.spec.as_ref().unwrap();
        spec.containers
            .iter()
            .chain(spec.init_containers.iter().flatten())
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no container named {name}"))
    }

    fn env_of(c: &Container, key: &str) -> Option<String> {
        c.env
            .as_ref()?
            .iter()
            .find(|e| e.name == key)
            .and_then(|e| e.value.clone())
    }

    #[test]
    fn names_the_pod_after_the_sandbox() {
        let pod = build(&sandbox(), &config()).unwrap();
        assert_eq!(pod.metadata.name.as_deref(), Some("sandbox-my-sandbox"));
        assert_eq!(pod.metadata.namespace.as_deref(), Some("team-a"));
    }

    #[test]
    fn sets_controller_owner_ref_to_the_sandbox() {
        let pod = build(&sandbox(), &config()).unwrap();
        let owners = pod.metadata.owner_references.as_ref().unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].kind, "Sandbox");
        assert_eq!(owners[0].name, "my-sandbox");
        assert_eq!(owners[0].controller, Some(true));
    }

    #[test]
    fn fails_without_uid_rather_than_emitting_an_unowned_pod() {
        let mut sb = sandbox();
        sb.metadata.uid = None;
        assert!(matches!(
            build(&sb, &config()),
            Err(PodBuildError::MissingObjectKey { key: "uid", .. })
        ));
    }

    #[test]
    fn labels_the_pod_for_the_daemon_selector() {
        let pod = build(&sandbox(), &config()).unwrap();
        let labels = pod.metadata.labels.as_ref().unwrap();
        assert_eq!(labels.get(SANDBOX_LABEL).map(String::as_str), Some("true"));
    }

    #[test]
    fn never_restarts_containers_itself() {
        let pod = build(&sandbox(), &config()).unwrap();
        assert_eq!(
            pod.spec.as_ref().unwrap().restart_policy.as_deref(),
            Some("Never")
        );
    }

    #[test]
    fn requests_exactly_one_kvm_device_on_the_runtime() {
        let pod = build(&sandbox(), &config()).unwrap();
        let limits = container(&pod, "msb-runtime")
            .resources
            .as_ref()
            .unwrap()
            .limits
            .as_ref()
            .unwrap();
        assert_eq!(limits.get(KVM_RESOURCE), Some(&Quantity("1".to_string())));
    }

    #[test]
    fn only_the_runtime_requests_kvm() {
        let pod = build(&sandbox(), &config()).unwrap();
        let spec = pod.spec.as_ref().unwrap();
        for c in spec
            .containers
            .iter()
            .chain(spec.init_containers.iter().flatten())
            .filter(|c| c.name != "msb-runtime")
        {
            let has_kvm = c
                .resources
                .as_ref()
                .and_then(|r| r.limits.as_ref())
                .is_some_and(|l| l.contains_key(KVM_RESOURCE));
            assert!(!has_kvm, "{} should not request KVM", c.name);
        }
    }

    #[test]
    fn runtime_memory_request_equals_limit_at_guest_plus_overhead() {
        // Fixture: 2 vCPU, 1024Mi guest. overhead = 96 + 1024/512 + 8*2 = 114.
        let pod = build(&sandbox(), &config()).unwrap();
        let res = container(&pod, "msb-runtime").resources.as_ref().unwrap();
        let req = res.requests.as_ref().unwrap();
        let lim = res.limits.as_ref().unwrap();
        let expected = Quantity("1138Mi".to_string());
        assert_eq!(req.get("memory"), Some(&expected));
        assert_eq!(lim.get("memory"), Some(&expected)); // Guaranteed for memory
    }

    #[test]
    fn runtime_cpu_requests_a_fraction_and_limits_to_vcpus() {
        let pod = build(&sandbox(), &config()).unwrap();
        let res = container(&pod, "msb-runtime").resources.as_ref().unwrap();
        assert_eq!(
            res.requests.as_ref().unwrap().get("cpu"),
            Some(&Quantity("200m".to_string())) // 2 vCPU / ratio 10
        );
        assert_eq!(
            res.limits.as_ref().unwrap().get("cpu"),
            Some(&Quantity("2".to_string())) // limited to the advertised vCPUs
        );
    }

    #[test]
    fn runtime_requests_ephemeral_storage() {
        let pod = build(&sandbox(), &config()).unwrap();
        let req = container(&pod, "msb-runtime")
            .resources
            .as_ref()
            .unwrap()
            .requests
            .as_ref()
            .unwrap();
        assert_eq!(
            req.get("ephemeral-storage"),
            Some(&Quantity("50Mi".to_string()))
        );
    }

    #[test]
    fn pod_sets_runtime_default_seccomp() {
        let pod = build(&sandbox(), &config()).unwrap();
        let sc = pod
            .spec
            .as_ref()
            .unwrap()
            .security_context
            .as_ref()
            .unwrap();
        assert_eq!(
            sc.seccomp_profile.as_ref().map(|p| p.type_.as_str()),
            Some("RuntimeDefault")
        );
    }

    #[test]
    fn bridge_uses_a_read_only_root_filesystem() {
        let pod = build(&sandbox(), &config()).unwrap();
        let c = container_or_init(&pod, "msb-bridge");
        assert_eq!(
            c.security_context
                .as_ref()
                .unwrap()
                .read_only_root_filesystem,
            Some(true),
            "the bridge should have a read-only rootfs"
        );
    }

    #[test]
    fn runs_non_root_in_the_kvm_group() {
        let pod = build(&sandbox(), &config()).unwrap();
        let sc = pod
            .spec
            .as_ref()
            .unwrap()
            .security_context
            .as_ref()
            .unwrap();
        assert_eq!(sc.run_as_non_root, Some(true));
        assert_eq!(sc.run_as_user, Some(1000));
        assert_eq!(sc.supplemental_groups, Some(vec![104]));
    }

    #[test]
    fn runtime_drops_all_caps_and_adds_none() {
        // msb's smoltcp net is pure userspace — no NET_ADMIN, no tap/tun.
        let pod = build(&sandbox(), &config()).unwrap();
        let caps = container(&pod, "msb-runtime")
            .security_context
            .as_ref()
            .unwrap()
            .capabilities
            .as_ref()
            .unwrap();
        assert_eq!(caps.drop, Some(vec!["ALL".to_string()]));
        assert_eq!(caps.add, None);
    }

    #[test]
    fn no_container_allows_privilege_escalation() {
        let pod = build(&sandbox(), &config()).unwrap();
        let spec = pod.spec.as_ref().unwrap();
        for c in spec
            .containers
            .iter()
            .chain(spec.init_containers.iter().flatten())
        {
            assert_eq!(
                c.security_context
                    .as_ref()
                    .unwrap()
                    .allow_privilege_escalation,
                Some(false),
                "{} allows privilege escalation",
                c.name
            );
        }
    }

    #[test]
    fn no_container_adds_any_capability() {
        let pod = build(&sandbox(), &config()).unwrap();
        let spec = pod.spec.as_ref().unwrap();
        for c in spec
            .containers
            .iter()
            .chain(spec.init_containers.iter().flatten())
        {
            let add = c
                .security_context
                .as_ref()
                .unwrap()
                .capabilities
                .as_ref()
                .unwrap()
                .add
                .clone();
            assert!(add.is_none(), "{} should add no capabilities", c.name);
        }
    }

    #[test]
    fn sets_msb_home_env_on_the_runtime() {
        let pod = build(&sandbox(), &config()).unwrap();
        assert_eq!(
            env_of(container(&pod, "msb-runtime"), "MSB_HOME"),
            Some("/msb".to_string())
        );
    }

    #[test]
    fn passes_the_spec_and_flat_name_to_the_runtime() {
        let pod = build(&sandbox(), &config()).unwrap();
        let rt = container(&pod, "msb-runtime");

        let encoded = env_of(rt, "MSB_SANDBOX_SPEC").expect("spec env var");
        let decoded: serde_json::Value =
            serde_json::from_str(&encoded).expect("round-trips as JSON");
        assert_eq!(decoded["image"], "python:3.12");
        assert_eq!(decoded["cpus"], 2);

        assert_eq!(
            env_of(rt, "MSB_SANDBOX_NAME"),
            Some("team-a__my-sandbox".to_string())
        );
    }

    #[test]
    fn stamps_rust_log_on_the_runtime() {
        let cfg = config().with_runtime_log("debug");
        let pod = build(&sandbox(), &cfg).unwrap();
        assert_eq!(
            env_of(container(&pod, "msb-runtime"), "RUST_LOG"),
            Some("debug".to_string())
        );
    }

    #[test]
    fn passes_the_spec_to_the_runtime() {
        let pod = build(&sandbox(), &config()).unwrap();
        let encoded =
            env_of(container(&pod, "msb-runtime"), "MSB_SANDBOX_SPEC").expect("spec env var");
        let decoded: serde_json::Value =
            serde_json::from_str(&encoded).expect("round-trips as JSON");
        assert_eq!(decoded["image"], "python:3.12");
        assert_eq!(decoded["cpus"], 2);
    }

    #[test]
    fn pod_uses_the_default_service_account() {
        // No Secret RBAC needed — the kubelet mounts referenced Secrets.
        let pod = build(&sandbox(), &config()).unwrap();
        assert_eq!(pod.spec.as_ref().unwrap().service_account_name, None);
    }

    #[test]
    fn mounts_one_volume_per_referenced_secret() {
        let pod = build(&sandbox_with_secrets(), &config()).unwrap();
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let secret_vols: Vec<_> = vols.iter().filter(|v| v.secret.is_some()).collect();
        // Two distinct Secrets (creds, db), one volume each.
        assert_eq!(secret_vols.len(), 2);

        let creds = secret_vols
            .iter()
            .find(|v| v.name == "secret-creds")
            .expect("creds volume");
        let src = creds.secret.as_ref().unwrap();
        assert_eq!(src.secret_name.as_deref(), Some("creds"));
        // Only the referenced keys are projected, each to a file named by key.
        let mut paths: Vec<_> = src
            .items
            .as_ref()
            .unwrap()
            .iter()
            .map(|i| (i.key.as_str(), i.path.as_str()))
            .collect();
        paths.sort();
        assert_eq!(paths, vec![("api", "api"), ("org", "org")]);
        assert_eq!(src.default_mode, Some(0o444));
    }

    #[test]
    fn runtime_mounts_each_secret_read_only_under_msb_secrets() {
        let pod = build(&sandbox_with_secrets(), &config()).unwrap();
        let mounts = container(&pod, "msb-runtime")
            .volume_mounts
            .as_ref()
            .unwrap();
        for (name, path) in [
            ("secret-creds", "/msb-secrets/creds"),
            ("secret-db", "/msb-secrets/db"),
        ] {
            let m = mounts.iter().find(|m| m.name == name).expect(name);
            assert_eq!(m.mount_path, path);
            assert_eq!(m.read_only, Some(true));
        }
    }

    #[test]
    fn no_secret_volumes_when_no_secrets() {
        let pod = build(&sandbox(), &config()).unwrap();
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        assert!(vols.iter().all(|v| v.secret.is_none()));
    }

    #[test]
    fn runtime_gets_no_rootfs_vmdk_env() {
        // The runtime boots from the app image against the injected cache, not a
        // vmdk path.
        let pod = build(&sandbox(), &config()).unwrap();
        assert_eq!(
            env_of(container(&pod, "msb-runtime"), "MSB_ROOTFS_VMDK"),
            None
        );
    }

    #[test]
    fn declares_only_the_home_volume() {
        // The cache is device-plugin-injected, so it never appears in the pod spec.
        let pod = build(&sandbox(), &config()).unwrap();
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let names: Vec<_> = vols.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec![VOL_HOME]);
    }

    #[test]
    fn home_is_a_per_pod_emptydir() {
        let pod = build(&sandbox(), &config()).unwrap();
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let home = vols.iter().find(|v| v.name == VOL_HOME).unwrap();
        assert!(home.empty_dir.is_some(), "home must be an emptyDir");
        assert!(home.host_path.is_none(), "home must not be a hostPath");
    }

    #[test]
    fn runtime_declares_no_cache_mount() {
        // The cache mount is device-plugin-injected, never declared here.
        let pod = build(&sandbox(), &config()).unwrap();
        let mounts = container(&pod, "msb-runtime")
            .volume_mounts
            .as_ref()
            .unwrap();
        assert!(
            mounts.iter().all(|m| m.mount_path != "/msb/cache"),
            "cache must not be a declared VolumeMount"
        );
    }

    #[test]
    fn bridge_exposes_the_configured_port() {
        let pod = build(&sandbox(), &config()).unwrap();
        let ports = container_or_init(&pod, "msb-bridge")
            .ports
            .as_ref()
            .unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].container_port, 7000);
        assert_eq!(ports[0].name.as_deref(), Some("agent"));
    }

    #[test]
    fn bridge_gets_the_sandbox_name_to_find_the_socket() {
        let pod = build(&sandbox(), &config()).unwrap();
        let bridge = container_or_init(&pod, "msb-bridge");
        assert_eq!(
            env_of(bridge, "MSB_SANDBOX_NAME").as_deref(),
            Some("team-a__my-sandbox")
        );
        assert_eq!(env_of(bridge, "MSB_HOME").as_deref(), Some("/msb"));
    }

    #[test]
    fn bridge_gets_the_home_read_only() {
        let pod = build(&sandbox(), &config()).unwrap();
        let mounts = container_or_init(&pod, "msb-bridge")
            .volume_mounts
            .as_ref()
            .unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, VOL_HOME);
        assert_eq!(mounts[0].read_only, Some(true));
    }

    #[test]
    fn runtime_gets_the_home_writable() {
        let pod = build(&sandbox(), &config()).unwrap();
        let mounts = container(&pod, "msb-runtime")
            .volume_mounts
            .as_ref()
            .unwrap();
        let home = mounts.iter().find(|m| m.name == VOL_HOME).unwrap();
        assert_ne!(home.read_only, Some(true), "runtime must write to MSB_HOME");
    }

    #[test]
    fn bridge_is_a_native_sidecar() {
        let pod = build(&sandbox(), &config()).unwrap();
        let spec = pod.spec.as_ref().unwrap();
        let inits = spec.init_containers.as_ref().unwrap();
        assert_eq!(inits.len(), 1, "only the bridge sidecar");
        assert_eq!(inits[0].name, "msb-bridge");
        assert_eq!(inits[0].restart_policy.as_deref(), Some("Always"));
        let names: Vec<_> = spec.containers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["msb-runtime"]);
    }
}
