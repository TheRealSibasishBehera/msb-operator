use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EmptyDirVolumeSource, EnvVar, HostPathVolumeSource,
    Pod, PodSecurityContext, PodSpec, ResourceRequirements, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Resource;
use msb_crd::Sandbox;

use crate::config::{BIN_MOUNT, CONFIG_MOUNT, ControllerConfig, KVM_RESOURCE, SANDBOX_LABEL};

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
const VOL_CONFIG: &str = "msb-config";
const VOL_BIN: &str = "msb-bin";

/// Sandbox pods run as this fixed non-root uid; `/root/.microsandbox` (msb's
/// default home) is unreadable to it, which is why MSB_HOME is set explicitly.
const RUN_AS_USER: i64 = 1000;

pub fn pod_name(sandbox_name: &str) -> String {
    format!("sandbox-{sandbox_name}")
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

    let labels = BTreeMap::from([
        (SANDBOX_LABEL.to_string(), "true".to_string()),
        ("microsandbox.io/sandbox-name".to_string(), name.clone()),
    ]);

    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(pod_name(&name)),
            namespace: Some(namespace),
            labels: Some(labels),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            init_containers: Some(vec![prerunner_container(cfg, &spec_json)]),
            containers: vec![runtime_container(cfg), bridge_container(cfg)],
            volumes: Some(volumes(cfg)),
            security_context: Some(PodSecurityContext {
                run_as_non_root: Some(true),
                run_as_user: Some(RUN_AS_USER),
                // The device plugin handles the cgroup allowlist but not Unix DAC;
                // /dev/kvm is crw-rw---- root:kvm, so the process must be in the group.
                supplemental_groups: Some(vec![cfg.kvm_gid]),
                ..Default::default()
            }),
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        status: None,
    })
}

fn prerunner_container(cfg: &ControllerConfig, spec_json: &str) -> Container {
    Container {
        name: "msb-prerunner".to_string(),
        image: Some(cfg.prerunner_image.clone()),
        env: Some(vec![EnvVar {
            name: "MSB_SANDBOX_SPEC".to_string(),
            value: Some(spec_json.to_string()),
            ..Default::default()
        }]),
        volume_mounts: Some(vec![
            VolumeMount {
                name: VOL_CONFIG.to_string(),
                mount_path: CONFIG_MOUNT.to_string(),
                ..Default::default()
            },
            VolumeMount {
                name: VOL_BIN.to_string(),
                mount_path: BIN_MOUNT.to_string(),
                ..Default::default()
            },
        ]),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                add: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn runtime_container(cfg: &ControllerConfig) -> Container {
    Container {
        name: "msb-runtime".to_string(),
        image: Some(cfg.runtime_image.clone()),
        env: Some(vec![
            EnvVar {
                name: "MSB_HOME".to_string(),
                value: Some(cfg.msb_home().to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "MSB_LIBKRUNFW_PATH".to_string(),
                value: Some(format!("{BIN_MOUNT}/libkrunfw.so")),
                ..Default::default()
            },
        ]),
        volume_mounts: Some(vec![
            VolumeMount {
                name: VOL_HOME.to_string(),
                mount_path: cfg.msb_home().to_string(),
                ..Default::default()
            },
            VolumeMount {
                name: VOL_CONFIG.to_string(),
                mount_path: CONFIG_MOUNT.to_string(),
                read_only: Some(true),
                ..Default::default()
            },
            VolumeMount {
                name: VOL_BIN.to_string(),
                mount_path: BIN_MOUNT.to_string(),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        resources: Some(ResourceRequirements {
            limits: Some(BTreeMap::from([(
                KVM_RESOURCE.to_string(),
                Quantity("1".to_string()),
            )])),
            ..Default::default()
        }),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                // libkrun needs NET_ADMIN for its virtio-net setup. smoltcp is pure
                // userspace and needs nothing; SYS_ADMIN is not required.
                add: Some(vec!["NET_ADMIN".to_string()]),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn bridge_container(cfg: &ControllerConfig) -> Container {
    Container {
        name: "msb-bridge".to_string(),
        image: Some(cfg.bridge_image.clone()),
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
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                add: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn volumes(cfg: &ControllerConfig) -> Vec<Volume> {
    vec![
        Volume {
            name: VOL_HOME.to_string(),
            host_path: Some(HostPathVolumeSource {
                path: cfg.msb_home().to_string(),
                type_: Some("DirectoryOrCreate".to_string()),
            }),
            ..Default::default()
        },
        Volume {
            name: VOL_CONFIG.to_string(),
            // tmpfs: holds fully-resolved secret values, keep them off node disk.
            empty_dir: Some(EmptyDirVolumeSource {
                medium: Some("Memory".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: VOL_BIN.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
    ]
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

    pub fn config() -> ControllerConfig {
        ControllerConfig::new(
            "/var/lib/msb",
            104,
            "ghcr.io/msb/prerunner:dev",
            "ghcr.io/msb/runtime:dev",
            "ghcr.io/msb/bridge:dev",
            7000,
        )
        .expect("valid test config")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{config, sandbox};
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
    fn has_prerunner_init_container_and_two_containers() {
        let pod = build(&sandbox(), &config()).unwrap();
        let spec = pod.spec.as_ref().unwrap();

        let inits = spec.init_containers.as_ref().unwrap();
        assert_eq!(inits.len(), 1);
        assert_eq!(inits[0].name, "msb-prerunner");

        let names: Vec<_> = spec.containers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["msb-runtime", "msb-bridge"]);
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
    fn runtime_drops_all_caps_and_adds_only_net_admin() {
        let pod = build(&sandbox(), &config()).unwrap();
        let caps = container(&pod, "msb-runtime")
            .security_context
            .as_ref()
            .unwrap()
            .capabilities
            .as_ref()
            .unwrap();
        assert_eq!(caps.drop, Some(vec!["ALL".to_string()]));
        assert_eq!(caps.add, Some(vec!["NET_ADMIN".to_string()]));
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
    fn only_the_runtime_gets_net_admin() {
        let pod = build(&sandbox(), &config()).unwrap();
        let spec = pod.spec.as_ref().unwrap();
        for c in spec
            .containers
            .iter()
            .chain(spec.init_containers.iter().flatten())
            .filter(|c| c.name != "msb-runtime")
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
    fn points_msb_home_at_the_hostpath() {
        let pod = build(&sandbox(), &config()).unwrap();
        assert_eq!(
            env_of(container(&pod, "msb-runtime"), "MSB_HOME"),
            Some("/var/lib/msb".to_string())
        );
    }

    #[test]
    fn points_libkrunfw_at_the_sideloaded_copy() {
        let pod = build(&sandbox(), &config()).unwrap();
        assert_eq!(
            env_of(container(&pod, "msb-runtime"), "MSB_LIBKRUNFW_PATH"),
            Some("/msb-bin/libkrunfw.so".to_string())
        );
    }

    #[test]
    fn passes_the_spec_to_the_prerunner() {
        let pod = build(&sandbox(), &config()).unwrap();
        let init = &pod.spec.as_ref().unwrap().init_containers.as_ref().unwrap()[0];
        let encoded = env_of(init, "MSB_SANDBOX_SPEC").expect("spec env var");
        let decoded: serde_json::Value =
            serde_json::from_str(&encoded).expect("round-trips as JSON");
        assert_eq!(decoded["image"], "python:3.12");
        assert_eq!(decoded["cpus"], 2);
    }

    #[test]
    fn declares_the_three_volumes() {
        let pod = build(&sandbox(), &config()).unwrap();
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let names: Vec<_> = vols.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec![VOL_HOME, VOL_CONFIG, VOL_BIN]);
    }

    #[test]
    fn home_is_a_hostpath_and_binaries_are_not() {
        let pod = build(&sandbox(), &config()).unwrap();
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let by_name = |n: &str| vols.iter().find(|v| v.name == n).unwrap();

        let home = by_name(VOL_HOME).host_path.as_ref().unwrap();
        assert_eq!(home.path, "/var/lib/msb");
        assert_eq!(home.type_.as_deref(), Some("DirectoryOrCreate"));

        assert!(by_name(VOL_BIN).host_path.is_none());
        assert!(by_name(VOL_BIN).empty_dir.is_some());
    }

    #[test]
    fn resolved_secrets_land_on_tmpfs_not_node_disk() {
        let pod = build(&sandbox(), &config()).unwrap();
        let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let config_vol = vols.iter().find(|v| v.name == VOL_CONFIG).unwrap();
        assert_eq!(
            config_vol.empty_dir.as_ref().unwrap().medium.as_deref(),
            Some("Memory")
        );
    }

    #[test]
    fn bridge_exposes_the_configured_port() {
        let pod = build(&sandbox(), &config()).unwrap();
        let ports = container(&pod, "msb-bridge").ports.as_ref().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].container_port, 7000);
        assert_eq!(ports[0].name.as_deref(), Some("agent"));
    }

    #[test]
    fn bridge_gets_the_home_read_only() {
        let pod = build(&sandbox(), &config()).unwrap();
        let mounts = container(&pod, "msb-bridge")
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
    fn prerunner_writes_config_and_binaries() {
        let pod = build(&sandbox(), &config()).unwrap();
        let init = &pod.spec.as_ref().unwrap().init_containers.as_ref().unwrap()[0];
        let mounts = init.volume_mounts.as_ref().unwrap();
        for m in mounts {
            assert_ne!(m.read_only, Some(true), "prerunner must write {}", m.name);
        }
        let names: Vec<_> = mounts.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec![VOL_CONFIG, VOL_BIN]);
    }
}
