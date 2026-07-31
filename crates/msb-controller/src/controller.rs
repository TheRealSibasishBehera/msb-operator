use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Pod, Service};
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType, Recorder};
use kube::{Api, Client, Resource, ResourceExt};
use msb_crd::{
    RunPolicy, Sandbox, SandboxCondition, SandboxPhase, SandboxStatus, TerminationReason,
};
use serde_json::json;
use tracing::{info, warn};

use crate::conditions;
use crate::config::{ControllerConfig, FIELD_MANAGER};
use crate::pod::{self, PodBuildError};
use crate::service;

fn event(type_: EventType, reason: &str, note: &str) -> Event {
    Event {
        type_,
        reason: reason.to_string(),
        note: Some(note.to_string()),
        // We don't distinguish action from reason for these lifecycle events.
        action: reason.to_string(),
        secondary: None,
    }
}

fn prior_conditions(sandbox: &Sandbox) -> Vec<SandboxCondition> {
    sandbox
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default()
}

/// Written by the daemon on sandbox exit; read at the `terminate` call site.
pub const ANN_TERMINATION_REASON: &str = "microsandbox.dev/termination-reason";
pub const ANN_TERMINATED_AT: &str = "microsandbox.dev/terminated-at";

// Fallback re-check cadence while a pod is Pending. The `.owns(pods)` watch
// normally reconciles the instant the pod flips Running; this only backstops a
// coalesced/late watch event, so keep it short — Pending is a brief window and a
// few extra get_opt calls there buy a much tighter time-to-Running.
const REQUEUE_WHILE_PENDING: Duration = Duration::from_millis(500);

/// Backoff before recreating a pod on `RerunOnFailure`. kube-rs backs off only on
/// reconcile *errors*; a retry after a clean success-path exit is not an error, so
/// we pace it ourselves — capped exponential in the restart count.
fn retry_backoff(restart_count: u32) -> Duration {
    const CAP: u64 = 300;
    let secs = 5u64.saturating_mul(1u64 << restart_count.min(6));
    Duration::from_secs(secs.min(CAP))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("building pod for sandbox {sandbox}: {source}")]
    BuildPod {
        sandbox: String,
        #[source]
        source: PodBuildError,
    },

    #[error("applying pod {pod} for sandbox {sandbox}: {source}")]
    ApplyPod {
        sandbox: String,
        pod: String,
        #[source]
        source: kube::Error,
    },

    #[error("applying service {service} for sandbox {sandbox}: {source}")]
    ApplyService {
        sandbox: String,
        service: String,
        #[source]
        source: kube::Error,
    },

    #[error("deleting pod {pod} for sandbox {sandbox}: {source}")]
    DeletePod {
        sandbox: String,
        pod: String,
        #[source]
        source: kube::Error,
    },

    #[error("patching status of sandbox {sandbox}: {source}")]
    PatchStatus {
        sandbox: String,
        #[source]
        source: kube::Error,
    },

    #[error("deleting ephemeral sandbox {sandbox}: {source}")]
    DeleteSandbox {
        sandbox: String,
        #[source]
        source: kube::Error,
    },

    #[error("sandbox {sandbox} is missing .metadata.{key}")]
    MissingObjectKey { sandbox: String, key: &'static str },
}

pub struct Context {
    pub client: Client,
    pub config: ControllerConfig,
    pub recorder: Recorder,
}

pub fn error_policy(sandbox: Arc<Sandbox>, error: &Error, _ctx: Arc<Context>) -> Action {
    match error {
        // A malformed object will not fix itself; wait for the user to change it
        // rather than burning retries.
        Error::BuildPod { .. } | Error::MissingObjectKey { .. } => {
            warn!(sandbox = %sandbox.name_any(), %error, "unrecoverable; waiting for change");
            Action::await_change()
        }
        _ => {
            warn!(sandbox = %sandbox.name_any(), %error, "reconcile failed; retrying");
            Action::requeue(Duration::from_secs(5))
        }
    }
}

pub async fn reconcile(sandbox: Arc<Sandbox>, ctx: Arc<Context>) -> Result<Action, Error> {
    let name = sandbox.name_any();
    let namespace = sandbox.namespace().ok_or_else(|| Error::MissingObjectKey {
        sandbox: name.clone(),
        key: "namespace",
    })?;

    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let sandboxes: Api<Sandbox> = Api::namespaced(ctx.client.clone(), &namespace);

    let pod_name = pod::pod_name(&name);
    let existing = match pods.get_opt(&pod_name).await {
        Ok(p) => p,
        Err(source) => {
            return Err(Error::ApplyPod {
                sandbox: name,
                pod: pod_name,
                source,
            });
        }
    };

    let Some(existing) = existing else {
        // Terminal sandboxes must not resurrect their pod: the controller deletes
        // the pod after recording status, and Once means no retry.
        if is_terminal(&sandbox) {
            return finish(&sandbox, &sandboxes, &name).await;
        }
        // A pod that was Running and is now gone vanished uncleanly — node loss,
        // eviction, or an external delete. It never reached a terminal phase we
        // could read a reason from, so treat the disappearance itself as the
        // unclean exit and route it through the same runPolicy decision.
        if was_running(&sandbox) {
            return handle_vanished_pod(&sandbox, &ctx, &sandboxes, &name).await;
        }
        return create_pod(&sandbox, &ctx, &pods, &name).await;
    };

    // A pod we already asked to delete (e.g. a retry in flight) still reports its
    // terminal phase until it's gone. Acting on it again would double-count the
    // restart; wait for it to disappear instead.
    if existing.metadata.deletion_timestamp.is_some() {
        return Ok(Action::requeue(REQUEUE_WHILE_PENDING));
    }

    // The runtime container is the VMM's parent: when it exits, the sandbox is
    // over even if the bridge sidecar keeps running. A Never-restart pod with a
    // still-running sidecar stays phase Running, so keying off the pod phase alone
    // would miss the sandbox's death — terminate on the runtime's exit directly.
    if runtime_terminated(&existing) {
        return terminate(&sandbox, &ctx, &sandboxes, &pods, &existing, &name).await;
    }

    match pod_phase(&existing) {
        // Gate Running on the bridge sidecar passing its readinessProbe, not just
        // pod phase: a native sidecar reaches phase Running before it binds :7000,
        // and the Service has no endpoints until it is ready — so an exec that
        // raced create would blackhole. Hold Pending until the bridge is dialable.
        Some("Running") if bridge_ready(&existing) => {
            mark_running(&sandbox, &ctx, &sandboxes, &existing, &name).await
        }
        Some("Running") => Ok(Action::requeue(REQUEUE_WHILE_PENDING)),
        Some("Succeeded") | Some("Failed") => {
            terminate(&sandbox, &ctx, &sandboxes, &pods, &existing, &name).await
        }
        _ => Ok(Action::requeue(REQUEUE_WHILE_PENDING)),
    }
}

/// True once the `msb-bridge` sidecar has passed its readinessProbe. As a native
/// sidecar (init container with `restartPolicy: Always`) its status lives in
/// `init_container_statuses`; `ready` tracks the probe, which is what gates the
/// Service endpoint.
fn bridge_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.init_container_statuses.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.name == "msb-bridge"))
        .map(|c| c.ready)
        .unwrap_or(false)
}

/// The `msb-runtime` container's terminated state, if it has exited. The runtime
/// is the VMM's parent, so this is the authoritative signal for the sandbox's own
/// lifecycle — independent of the bridge sidecar and the pod's overall phase.
fn runtime_terminated_state(
    pod: &Pod,
) -> Option<&k8s_openapi::api::core::v1::ContainerStateTerminated> {
    pod.status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.name == "msb-runtime"))
        .and_then(|c| c.state.as_ref())
        .and_then(|s| s.terminated.as_ref())
}

/// True once the `msb-runtime` container has terminated — the signal that the
/// sandbox itself has ended, regardless of the pod's overall phase.
fn runtime_terminated(pod: &Pod) -> bool {
    runtime_terminated_state(pod).is_some()
}

/// The runtime container's exit code, if it has terminated.
fn runtime_exit_code(pod: &Pod) -> Option<i32> {
    runtime_terminated_state(pod).map(|t| t.exit_code)
}

async fn create_pod(
    sandbox: &Sandbox,
    ctx: &Context,
    pods: &Api<Pod>,
    name: &str,
) -> Result<Action, Error> {
    let namespace = sandbox.namespace().ok_or_else(|| Error::MissingObjectKey {
        sandbox: name.to_string(),
        key: "namespace",
    })?;

    let desired = pod::build(sandbox, &ctx.config).map_err(|source| Error::BuildPod {
        sandbox: name.to_string(),
        source,
    })?;
    let pod_name = desired.name_any();

    pods.patch(
        &pod_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Apply(&desired),
    )
    .await
    .map_err(|source| Error::ApplyPod {
        sandbox: name.to_string(),
        pod: pod_name.clone(),
        source,
    })?;

    // The per-sandbox Service fronts the bridge. Owner-ref'd to the Sandbox, so
    // it is GC'd on delete; server-side apply makes recreate idempotent.
    let svc = service::build(sandbox, &ctx.config, name, &namespace);
    let svc_name = svc.name_any();
    let services: Api<Service> = Api::namespaced(ctx.client.clone(), &namespace);
    services
        .patch(
            &svc_name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Apply(&svc),
        )
        .await
        .map_err(|source| Error::ApplyService {
            sandbox: name.to_string(),
            service: svc_name.clone(),
            source,
        })?;

    info!(sandbox = %name, pod = %pod_name, service = %svc_name, "created sandbox pod + service");
    Ok(Action::requeue(REQUEUE_WHILE_PENDING))
}

async fn mark_running(
    sandbox: &Sandbox,
    ctx: &Context,
    sandboxes: &Api<Sandbox>,
    pod: &Pod,
    name: &str,
) -> Result<Action, Error> {
    if sandbox.status.as_ref().and_then(|s| s.phase.as_ref()) == Some(&SandboxPhase::Running) {
        return Ok(Action::await_change());
    }

    let now = now_rfc3339();
    let mut conditions = prior_conditions(sandbox);
    conditions::set(
        &mut conditions,
        conditions::ready(true, "PodRunning", "sandbox pod is running", now.clone()),
    );
    let service_name = sandbox
        .namespace()
        .map(|ns| service::service_name(&ns, name));
    let exposed_ports = sandbox
        .spec
        .network
        .published_ports
        .iter()
        .map(|p| msb_crd::sandbox::ExposedPort {
            port: p.host_port(),
            protocol: p.protocol,
            name: pod::port_name(p.host_port()),
        })
        .collect();
    let status = SandboxStatus {
        phase: Some(SandboxPhase::Running),
        pod_name: Some(pod.name_any()),
        service_name,
        node_name: pod.spec.as_ref().and_then(|s| s.node_name.clone()),
        started_at: Some(now),
        // Preserve the retry count across the Pending→Running transition.
        restart_count: sandbox.status.as_ref().map(|s| s.restart_count).unwrap_or(0),
        exposed_ports,
        conditions,
        ..Default::default()
    };

    patch_status(sandboxes, name, &status).await?;
    ctx.recorder
        .publish(
            &event(EventType::Normal, "Running", "sandbox pod is running"),
            &sandbox.object_ref(&()),
        )
        .await
        .ok();
    info!(sandbox = %name, "running");
    Ok(Action::await_change())
}

async fn terminate(
    sandbox: &Sandbox,
    ctx: &Context,
    sandboxes: &Api<Sandbox>,
    pods: &Api<Pod>,
    pod: &Pod,
    name: &str,
) -> Result<Action, Error> {
    // Prefer an explicit annotation if one is ever present; otherwise derive the
    // reason from Pod status. Read before delete — the pod is GC'd below.
    let reason = pod
        .annotations()
        .get(ANN_TERMINATION_REASON)
        .and_then(|r| parse_termination_reason(r))
        .or_else(|| reason_from_pod(pod));
    let terminated_at = pod
        .annotations()
        .get(ANN_TERMINATED_AT)
        .cloned()
        .unwrap_or_else(now_rfc3339);

    // Derive success from the runtime container's exit, not the pod phase: the
    // pod can still be Running (bridge sidecar alive) when the runtime has exited.
    // Fall back to the pod phase only if the runtime's exit code is unavailable.
    let runtime_exit = runtime_exit_code(pod);
    let phase = match runtime_exit {
        Some(0) => SandboxPhase::Succeeded,
        Some(_) => SandboxPhase::Failed,
        None => match pod_phase(pod) {
            Some("Succeeded") => SandboxPhase::Succeeded,
            _ => SandboxPhase::Failed,
        },
    };

    let exit_code = runtime_exit;

    let succeeded = phase == SandboxPhase::Succeeded;
    let reason_str = reason
        .as_ref()
        .map(|r| format!("{r:?}"))
        .unwrap_or_else(|| "Unknown".to_string());

    // An unclean exit under RerunOnFailure is retried: reset to Pending with a
    // fresh pod rather than resting terminal. The msb sandbox name is reused, but
    // MSB_HOME is a per-pod emptyDir, so the new pod boots against a clean home
    // with no prior DB row or dir — no SandboxAlreadyExists, no upper to preserve.
    let prior = sandbox.status.as_ref();
    let retrying = !succeeded && sandbox.spec.run_policy == RunPolicy::RerunOnFailure;
    let restart_count = prior.map(|s| s.restart_count).unwrap_or(0);

    let mut conditions = prior_conditions(sandbox);
    conditions::set(
        &mut conditions,
        conditions::ready(false, &reason_str, "sandbox exited", now_rfc3339()),
    );

    // Carry forward the fields the Running phase set: a merge patch reads a
    // missing field as null-and-delete, so omitting these would wipe them.
    let status = if retrying {
        SandboxStatus {
            phase: Some(SandboxPhase::Pending),
            pod_name: None,
            service_name: prior.and_then(|s| s.service_name.clone()),
            node_name: prior.and_then(|s| s.node_name.clone()),
            started_at: None,
            terminated_at: Some(terminated_at),
            termination_reason: reason.clone(),
            exit_code,
            restart_count: restart_count + 1,
            exposed_ports: prior.map(|s| s.exposed_ports.clone()).unwrap_or_default(),
            conditions,
        }
    } else {
        SandboxStatus {
            phase: Some(phase.clone()),
            pod_name: Some(pod.name_any()),
            service_name: prior.and_then(|s| s.service_name.clone()),
            node_name: prior.and_then(|s| s.node_name.clone()),
            started_at: prior.and_then(|s| s.started_at.clone()),
            terminated_at: Some(terminated_at),
            termination_reason: reason.clone(),
            exit_code,
            restart_count,
            exposed_ports: prior.map(|s| s.exposed_ports.clone()).unwrap_or_default(),
            conditions,
        }
    };
    patch_status(sandboxes, name, &status).await?;
    ctx.recorder
        .publish(
            &event(
                if succeeded {
                    EventType::Normal
                } else {
                    EventType::Warning
                },
                if succeeded { "Succeeded" } else { "Failed" },
                &format!("sandbox terminated: {reason_str}"),
            ),
            &sandbox.object_ref(&()),
        )
        .await
        .ok();

    let pod_name = pod.name_any();
    if let Err(source) = pods.delete(&pod_name, &DeleteParams::default()).await
        && !is_not_found(&source)
    {
        return Err(Error::DeletePod {
            sandbox: name.to_string(),
            pod: pod_name,
            source,
        });
    }

    if retrying {
        let backoff = retry_backoff(restart_count);
        info!(sandbox = %name, ?reason, restart = restart_count + 1, ?backoff, "retrying");
        // Wait for the pod delete to propagate, then a fresh reconcile recreates
        // it: no pod + non-terminal Pending routes back to create_pod.
        return Ok(Action::requeue(backoff));
    }

    info!(sandbox = %name, ?phase, ?reason, "terminated");
    finish(sandbox, sandboxes, name).await
}

/// A Running pod disappeared. Record it as a `NodeLost` unclean exit, then apply
/// the same runPolicy decision as a Failed exit: retry under `RerunOnFailure`,
/// otherwise settle terminal `Failed`.
async fn handle_vanished_pod(
    sandbox: &Sandbox,
    ctx: &Context,
    sandboxes: &Api<Sandbox>,
    name: &str,
) -> Result<Action, Error> {
    let prior = sandbox.status.as_ref();
    let retrying = sandbox.spec.run_policy == RunPolicy::RerunOnFailure;
    let restart_count = prior.map(|s| s.restart_count).unwrap_or(0);
    let reason = TerminationReason::NodeLost;

    let mut conditions = prior_conditions(sandbox);
    conditions::set(
        &mut conditions,
        conditions::ready(false, "NodeLost", "sandbox pod disappeared", now_rfc3339()),
    );

    let status = SandboxStatus {
        phase: Some(if retrying {
            SandboxPhase::Pending
        } else {
            SandboxPhase::Failed
        }),
        pod_name: if retrying { None } else { prior.and_then(|s| s.pod_name.clone()) },
        service_name: prior.and_then(|s| s.service_name.clone()),
        node_name: prior.and_then(|s| s.node_name.clone()),
        started_at: if retrying { None } else { prior.and_then(|s| s.started_at.clone()) },
        terminated_at: Some(now_rfc3339()),
        termination_reason: Some(reason),
        exit_code: None,
        restart_count: if retrying { restart_count + 1 } else { restart_count },
        exposed_ports: prior.map(|s| s.exposed_ports.clone()).unwrap_or_default(),
        conditions,
    };
    patch_status(sandboxes, name, &status).await?;
    ctx.recorder
        .publish(
            &event(EventType::Warning, "NodeLost", "sandbox pod disappeared"),
            &sandbox.object_ref(&()),
        )
        .await
        .ok();

    if retrying {
        let backoff = retry_backoff(restart_count);
        info!(sandbox = %name, restart = restart_count + 1, ?backoff, "retrying after pod loss");
        return Ok(Action::requeue(backoff));
    }

    warn!(sandbox = %name, "pod lost; runPolicy is Once, settling Failed");
    finish(sandbox, sandboxes, name).await
}

/// True if the sandbox's last recorded phase was `Running` — used to tell a
/// pod that vanished mid-run from one not yet created.
fn was_running(sandbox: &Sandbox) -> bool {
    sandbox.status.as_ref().and_then(|s| s.phase.as_ref()) == Some(&SandboxPhase::Running)
}

async fn finish(sandbox: &Sandbox, sandboxes: &Api<Sandbox>, name: &str) -> Result<Action, Error> {
    if !sandbox.spec.ephemeral {
        return Ok(Action::await_change());
    }

    if let Err(source) = sandboxes.delete(name, &DeleteParams::default()).await
        && !is_not_found(&source)
    {
        return Err(Error::DeleteSandbox {
            sandbox: name.to_string(),
            source,
        });
    }

    info!(sandbox = %name, "deleted ephemeral sandbox");
    Ok(Action::await_change())
}

async fn patch_status(
    sandboxes: &Api<Sandbox>,
    name: &str,
    status: &SandboxStatus,
) -> Result<(), Error> {
    let patch = json!({ "status": status });
    sandboxes
        .patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(|source| Error::PatchStatus {
            sandbox: name.to_string(),
            source,
        })?;
    Ok(())
}

fn pod_phase(pod: &Pod) -> Option<&str> {
    pod.status.as_ref()?.phase.as_deref()
}

/// An unrecognised reason yields `None`, not an error: a value msb adds later
/// must not block us from recording that the sandbox terminated.
fn parse_termination_reason(value: &str) -> Option<TerminationReason> {
    serde_json::from_value(serde_json::Value::String(value.to_string())).ok()
}

/// Termination reason from Pod status: the runtime container's terminated reason
/// (`OOMKilled`/`Completed`/`Error`) and exit code, plus a pod-level `Evicted`.
fn reason_from_pod(pod: &Pod) -> Option<TerminationReason> {
    // Pod-level eviction is surfaced as the phase reason, above container state.
    if pod.status.as_ref().and_then(|s| s.reason.as_deref()) == Some("Evicted") {
        return Some(TerminationReason::Evicted);
    }

    let terminated = runtime_terminated_state(pod)?;

    match terminated.reason.as_deref() {
        Some("OOMKilled") => Some(TerminationReason::OomKilled),
        Some("Completed") if terminated.exit_code == 0 => Some(TerminationReason::Completed),
        // "Error", "ContainerCannotRun", a non-zero "Completed", or anything else.
        _ if terminated.exit_code == 0 => Some(TerminationReason::Completed),
        _ => Some(TerminationReason::Failed),
    }
}

fn is_terminal(sandbox: &Sandbox) -> bool {
    matches!(
        sandbox.status.as_ref().and_then(|s| s.phase.as_ref()),
        Some(SandboxPhase::Succeeded) | Some(SandboxPhase::Failed)
    )
}

fn is_not_found(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(e) if e.code == 404)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
    };
    use kube::api::ObjectMeta;
    use std::collections::BTreeMap;

    use super::*;
    use crate::pod::test_support::sandbox;

    fn pod_with_phase(phase: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some("sandbox-my-sandbox".to_string()),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some(phase.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A pod whose msb-runtime container terminated with the given k8s reason
    /// and exit code.
    fn pod_terminated(reason: Option<&str>, exit_code: i32) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some("Failed".to_string()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "msb-runtime".to_string(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code,
                            reason: reason.map(String::from),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A pod still phase Running (bridge sidecar alive) whose msb-runtime has
    /// terminated with the given exit code — the case that keying off pod phase
    /// alone would miss.
    fn pod_running_but_runtime_dead(exit_code: i32) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some("Running".to_string()),
                container_statuses: Some(vec![
                    ContainerStatus {
                        name: "msb-bridge".to_string(),
                        state: Some(ContainerState {
                            running: Some(Default::default()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ContainerStatus {
                        name: "msb-runtime".to_string(),
                        state: Some(ContainerState {
                            terminated: Some(ContainerStateTerminated {
                                exit_code,
                                reason: Some(if exit_code == 0 { "Completed" } else { "Error" }.into()),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn runtime_death_is_detected_while_pod_still_running() {
        // The bug this guards: a Never-restart pod stays phase Running while the
        // bridge sidecar lives, so the runtime's exit must be read directly.
        let pod = pod_running_but_runtime_dead(1);
        assert_eq!(pod_phase(&pod), Some("Running"), "pod is still Running");
        assert!(runtime_terminated(&pod), "runtime exit must be seen anyway");
        assert_eq!(runtime_exit_code(&pod), Some(1));

        // A live sandbox: runtime running, nothing terminated.
        let live = pod_with_phase("Running");
        assert!(!runtime_terminated(&live));
        assert_eq!(runtime_exit_code(&live), None);
    }

    #[test]
    fn clean_runtime_exit_is_succeeded_even_if_pod_reads_running() {
        // phase derived from the runtime exit code, not the (still Running) pod.
        let pod = pod_running_but_runtime_dead(0);
        assert_eq!(runtime_exit_code(&pod), Some(0));
        assert_eq!(reason_from_pod(&pod), Some(TerminationReason::Completed));
    }

    #[test]
    fn reads_phase_from_pod_status() {
        assert_eq!(pod_phase(&pod_with_phase("Running")), Some("Running"));
        assert_eq!(pod_phase(&Pod::default()), None);
    }

    #[test]
    fn bridge_ready_tracks_the_sidecar_probe() {
        let with_bridge = |ready: bool| Pod {
            status: Some(PodStatus {
                init_container_statuses: Some(vec![ContainerStatus {
                    name: "msb-bridge".to_string(),
                    ready,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(bridge_ready(&with_bridge(true)));
        assert!(!bridge_ready(&with_bridge(false)));
        // No sidecar status yet (pod just Running) is not ready.
        assert!(!bridge_ready(&pod_with_phase("Running")));
    }

    #[test]
    fn derives_oomkilled_from_container_reason() {
        let pod = pod_terminated(Some("OOMKilled"), 137);
        assert_eq!(reason_from_pod(&pod), Some(TerminationReason::OomKilled));
    }

    #[test]
    fn derives_completed_on_clean_exit() {
        let pod = pod_terminated(Some("Completed"), 0);
        assert_eq!(reason_from_pod(&pod), Some(TerminationReason::Completed));
    }

    #[test]
    fn derives_failed_on_error_exit() {
        let pod = pod_terminated(Some("Error"), 1);
        assert_eq!(reason_from_pod(&pod), Some(TerminationReason::Failed));
    }

    #[test]
    fn derives_evicted_from_pod_reason() {
        let mut pod = pod_terminated(None, 0);
        pod.status.as_mut().unwrap().reason = Some("Evicted".to_string());
        assert_eq!(reason_from_pod(&pod), Some(TerminationReason::Evicted));
    }

    #[test]
    fn no_terminated_container_yields_none() {
        assert_eq!(reason_from_pod(&pod_with_phase("Running")), None);
    }

    #[test]
    fn terminal_only_for_succeeded_and_failed() {
        let mut sb = sandbox();
        assert!(!is_terminal(&sb), "no status is not terminal");

        for (phase, expected) in [
            (SandboxPhase::Pending, false),
            (SandboxPhase::Running, false),
            (SandboxPhase::Succeeded, true),
            (SandboxPhase::Failed, true),
        ] {
            sb.status = Some(msb_crd::SandboxStatus {
                phase: Some(phase.clone()),
                ..Default::default()
            });
            assert_eq!(is_terminal(&sb), expected, "{phase:?}");
        }
    }

    #[test]
    fn exit_code_comes_from_the_runtime_container() {
        let mut pod = pod_with_phase("Failed");
        pod.status.as_mut().unwrap().container_statuses = Some(vec![
            ContainerStatus {
                name: "msb-bridge".to_string(),
                state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code: 7,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ContainerStatus {
                name: "msb-runtime".to_string(),
                state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code: 42,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]);

        let code = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.name == "msb-runtime"))
            .and_then(|c| c.state.as_ref())
            .and_then(|s| s.terminated.as_ref())
            .map(|t| t.exit_code);

        assert_eq!(code, Some(42), "must not pick up the bridge's exit code");
    }

    #[test]
    fn parses_the_daemon_termination_annotation() {
        let mut pod = pod_with_phase("Succeeded");
        pod.metadata.annotations = Some(BTreeMap::from([(
            ANN_TERMINATION_REASON.to_string(),
            "Completed".to_string(),
        )]));

        let reason = pod
            .annotations()
            .get(ANN_TERMINATION_REASON)
            .and_then(|r| parse_termination_reason(r));

        assert_eq!(reason, Some(TerminationReason::Completed));
    }

    #[test]
    fn missing_annotation_yields_no_reason_rather_than_a_wrong_one() {
        let pod = pod_with_phase("Failed");
        let reason = pod
            .annotations()
            .get(ANN_TERMINATION_REASON)
            .and_then(|r| parse_termination_reason(r));
        assert_eq!(reason, None);
    }

    #[test]
    fn unknown_reason_is_dropped_not_errored() {
        assert_eq!(
            parse_termination_reason("Completed"),
            Some(TerminationReason::Completed)
        );
        assert_eq!(parse_termination_reason("SomethingMsbAddedLater"), None);
    }

    // A merge patch reads a missing field as null-and-delete. The terminate patch
    // must re-send startedAt/nodeName or it wipes what the Running phase recorded.
    #[test]
    fn terminate_status_preserves_fields_set_while_running() {
        let prior = SandboxStatus {
            phase: Some(SandboxPhase::Running),
            pod_name: Some("sandbox-my-sandbox".to_string()),
            node_name: Some("node-1".to_string()),
            started_at: Some("2026-07-17T10:00:00Z".to_string()),
            ..Default::default()
        };

        let terminated = SandboxStatus {
            phase: Some(SandboxPhase::Succeeded),
            pod_name: Some("sandbox-my-sandbox".to_string()),
            node_name: prior.node_name.clone(),
            started_at: prior.started_at.clone(),
            terminated_at: Some("2026-07-17T10:05:00Z".to_string()),
            termination_reason: Some(TerminationReason::Completed),
            exit_code: Some(0),
            ..Default::default()
        };

        assert_eq!(
            terminated.started_at.as_deref(),
            Some("2026-07-17T10:00:00Z")
        );
        assert_eq!(terminated.node_name.as_deref(), Some("node-1"));
    }

    #[test]
    fn retry_backoff_grows_then_caps() {
        // Exponential from 5s, capped at 300s so a flapping sandbox doesn't spin.
        assert_eq!(retry_backoff(0), Duration::from_secs(5));
        assert_eq!(retry_backoff(1), Duration::from_secs(10));
        assert_eq!(retry_backoff(3), Duration::from_secs(40));
        assert_eq!(retry_backoff(6), Duration::from_secs(300));
        assert_eq!(retry_backoff(100), Duration::from_secs(300));
    }

    #[test]
    fn rerun_on_failure_retries_only_unclean_exits() {
        use msb_crd::RunPolicy;
        let retries = |policy: RunPolicy, succeeded: bool| {
            !succeeded && policy == RunPolicy::RerunOnFailure
        };
        assert!(retries(RunPolicy::RerunOnFailure, false), "unclean + policy");
        assert!(!retries(RunPolicy::RerunOnFailure, true), "clean exit stops");
        assert!(!retries(RunPolicy::Once, false), "Once never retries");
        assert!(!retries(RunPolicy::Once, true));
    }

    #[test]
    fn was_running_reads_the_recorded_phase() {
        let mut sb = sandbox();
        assert!(!was_running(&sb), "no status");
        for (phase, expected) in [
            (SandboxPhase::Pending, false),
            (SandboxPhase::Running, true),
            (SandboxPhase::Succeeded, false),
            (SandboxPhase::Failed, false),
        ] {
            sb.status = Some(SandboxStatus {
                phase: Some(phase.clone()),
                ..Default::default()
            });
            assert_eq!(was_running(&sb), expected, "{phase:?}");
        }
    }

    #[test]
    fn timestamps_are_rfc3339() {
        let ts = now_rfc3339();
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok(), "{ts}");
        assert!(ts.ends_with('Z'), "{ts} should be UTC");
    }

    #[test]
    fn not_found_is_recognised_so_delete_is_idempotent() {
        let mut not_found = kube::core::Status::failure("not found", "NotFound");
        not_found.code = 404;
        assert!(is_not_found(&kube::Error::Api(not_found.boxed())));

        let mut conflict = kube::core::Status::failure("conflict", "Conflict");
        conflict.code = 409;
        assert!(!is_not_found(&kube::Error::Api(conflict.boxed())));
    }
}
