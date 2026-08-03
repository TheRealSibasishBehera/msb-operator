use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType, Recorder};
use kube::{Api, Client, Resource, ResourceExt};
use msb_crd::{RunPolicy, Sandbox, SandboxPhase, SandboxStatus, ShutdownPolicy, TerminationReason};
use serde_json::json;
use tracing::{info, warn};

use crate::conditions;
use crate::config::{ControllerConfig, FIELD_MANAGER};
use crate::pod::{self, PodBuildError};
use crate::resize;
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

fn prior_conditions(sandbox: &Sandbox) -> Vec<Condition> {
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
    /// Reaches the per-sandbox bridge's `/control` relay for live resizes.
    pub http: reqwest::Client,
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

    // Expiry preempts the normal flow, so a pod deleted here is never misread as
    // a vanished-pod failure.
    if is_expired(&sandbox) {
        return expire(&sandbox, &ctx, &sandboxes, &pods, &name).await;
    }

    let stopped = desired_stopped(&sandbox);

    let Some(existing) = existing else {
        if stopped {
            return mark_stopped(&sandbox, &ctx, &sandboxes, &name).await;
        }
        // Terminal sandboxes must not resurrect their pod: the controller deletes
        // the pod after recording status, and Once means no retry.
        if is_terminal(&sandbox) {
            return Ok(finish());
        }
        // A pod that was Running and is now gone vanished uncleanly — node loss,
        // eviction, or an external delete. It never reached a terminal phase we
        // could read a reason from, so treat the disappearance itself as the
        // unclean exit and route it through the same runPolicy decision. A desired
        // Stop lands in phase Stopped, not Running, so it does not trip this.
        if was_running(&sandbox) {
            return handle_vanished_pod(&sandbox, &ctx, &sandboxes, &name).await;
        }
        if let Some(port) = service::reserved_port_conflict(&sandbox, &ctx.config) {
            return mark_port_conflict(&sandbox, &ctx, &sandboxes, &name, port).await;
        }
        return create_pod(&sandbox, &ctx, &pods, &name).await;
    };

    if stopped {
        // Guard the delete so a reconcile during termination doesn't re-issue it.
        if existing.metadata.deletion_timestamp.is_none() {
            delete_pod(&pods, &existing.name_any(), &name).await?;
        }
        return mark_stopped(&sandbox, &ctx, &sandboxes, &name).await;
    }

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
        return reconcile_resize(sandbox, ctx, sandboxes, name).await;
    }

    let now = now_time();
    let mut conditions = prior_conditions(sandbox);
    conditions::set(
        &mut conditions,
        conditions::ready(
            true,
            "PodRunning",
            "sandbox pod is running",
            sandbox.metadata.generation,
            now.clone(),
        ),
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
        restart_count: sandbox
            .status
            .as_ref()
            .map(|s| s.restart_count)
            .unwrap_or(0),
        exposed_ports,
        applied_cpus: Some(sandbox.spec.cpus),
        applied_memory: Some(sandbox.spec.memory),
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
    Ok(running_action(sandbox))
}

/// Requeue at the `shutdownTime` deadline (the watch wouldn't wake us for it),
/// else await the next change.
fn running_action(sandbox: &Sandbox) -> Action {
    match shutdown_deadline(sandbox) {
        Some(deadline) => Action::requeue(requeue_until(&deadline)),
        None => Action::await_change(),
    }
}

/// Whether `spec.cpus`/`spec.memory` differ from what was last confirmed
/// applied — the signal that a resize is pending.
fn resize_pending(sandbox: &Sandbox) -> bool {
    let Some(status) = sandbox.status.as_ref() else {
        return false;
    };
    status.applied_cpus != Some(sandbox.spec.cpus)
        || status.applied_memory != Some(sandbox.spec.memory)
}

/// Applies a pending `spec.cpus`/`spec.memory` edit to an already-Running
/// sandbox: live over the bridge control endpoint when the sandbox booted with
/// hotplug headroom, else `RestartRequired` — never a silent no-op, never an
/// automatic restart (the current storage model loses guest state on one).
async fn reconcile_resize(
    sandbox: &Sandbox,
    ctx: &Context,
    sandboxes: &Api<Sandbox>,
    name: &str,
) -> Result<Action, Error> {
    if !resize_pending(sandbox) {
        return clear_restart_required(sandbox, sandboxes, name).await;
    }

    let now = now_time();
    let mut conditions = prior_conditions(sandbox);

    // A dimension with no reserved headroom can't hotplug (its control listener
    // never spawned), so a change to it needs a restart. Skip the doomed attempt.
    let cpus_changed =
        sandbox.status.as_ref().and_then(|s| s.applied_cpus) != Some(sandbox.spec.cpus);
    let memory_changed =
        sandbox.status.as_ref().and_then(|s| s.applied_memory) != Some(sandbox.spec.memory);
    if (cpus_changed && !resize::cpus_have_headroom(&sandbox.spec))
        || (memory_changed && !resize::memory_has_headroom(&sandbox.spec))
    {
        conditions::set(
            &mut conditions,
            conditions::restart_required(
                true,
                "NoHotplugHeadroom",
                "spec.cpus/spec.memory changed but the sandbox booted without hotplug headroom \
                 (set maxCpus/maxMemory above cpus/memory at creation); restart the sandbox \
                 (desiredState Stopped then Running) to apply",
                sandbox.metadata.generation,
                now,
            ),
        );
        let status = json!({ "status": { "conditions": conditions } });
        patch_status_merge(sandboxes, name, &status).await?;
        warn!(sandbox = %name, "resize needs headroom the sandbox lacks; restart required");
        return Ok(running_action(sandbox));
    }

    let Some(service_name) = sandbox.status.as_ref().and_then(|s| s.service_name.clone()) else {
        return Ok(Action::requeue(REQUEUE_WHILE_PENDING));
    };
    let namespace = sandbox.namespace().ok_or_else(|| Error::MissingObjectKey {
        sandbox: name.to_string(),
        key: "namespace",
    })?;
    let url = resize::control_url(&service_name, &namespace, ctx.config.bridge_control_port);

    let applied_cpus = sandbox.status.as_ref().and_then(|s| s.applied_cpus);
    let applied_memory = sandbox.status.as_ref().and_then(|s| s.applied_memory);

    let mut new_applied_cpus = applied_cpus;
    let mut new_applied_memory = applied_memory;
    let mut live_error = None;

    if applied_cpus != Some(sandbox.spec.cpus) {
        match resize::apply(
            &ctx.http,
            &url,
            &resize::cpu_target_request(sandbox.spec.cpus),
        )
        .await
        {
            Ok(_) => new_applied_cpus = Some(sandbox.spec.cpus),
            Err(e) => live_error = Some(e.to_string()),
        }
    }
    if live_error.is_none() && applied_memory != Some(sandbox.spec.memory) {
        match resize::apply(
            &ctx.http,
            &url,
            &resize::memory_target_request(sandbox.spec.memory),
        )
        .await
        {
            Ok(_) => new_applied_memory = Some(sandbox.spec.memory),
            Err(e) => live_error = Some(e.to_string()),
        }
    }

    if let Some(error) = live_error {
        conditions::set(
            &mut conditions,
            conditions::restart_required(
                true,
                "ControlSocketUnreachable",
                &format!(
                    "live resize failed ({error}); restart the sandbox (desiredState Stopped \
                     then Running) to apply spec.cpus/spec.memory"
                ),
                sandbox.metadata.generation,
                now,
            ),
        );
        warn!(sandbox = %name, %error, "live resize failed; restart required");
    } else {
        conditions::set(
            &mut conditions,
            conditions::restart_required(
                false,
                "Resized",
                "resize applied live",
                sandbox.metadata.generation,
                now,
            ),
        );
        info!(sandbox = %name, cpus = new_applied_cpus, memory = new_applied_memory, "resized live");
    }

    let status = json!({
        "status": {
            "appliedCpus": new_applied_cpus,
            "appliedMemory": new_applied_memory,
            "conditions": conditions,
        }
    });
    patch_status_merge(sandboxes, name, &status).await?;
    Ok(running_action(sandbox))
}

/// Clear a stale `RestartRequired: True` once its resize was applied. No-op
/// unless the flag is set.
async fn clear_restart_required(
    sandbox: &Sandbox,
    sandboxes: &Api<Sandbox>,
    name: &str,
) -> Result<Action, Error> {
    let stale = sandbox
        .status
        .as_ref()
        .map(|s| &s.conditions)
        .into_iter()
        .flatten()
        .any(|c| c.type_ == conditions::RESTART_REQUIRED && c.status == "True");
    if !stale {
        return Ok(running_action(sandbox));
    }

    let mut conditions = prior_conditions(sandbox);
    conditions::set(
        &mut conditions,
        conditions::restart_required(
            false,
            "Resized",
            "resize applied",
            sandbox.metadata.generation,
            now_time(),
        ),
    );
    let status = json!({ "status": { "conditions": conditions } });
    patch_status_merge(sandboxes, name, &status).await?;
    Ok(running_action(sandbox))
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
        .and_then(|s| s.parse().ok().map(Time))
        .unwrap_or_else(now_time);

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
        conditions::ready(
            false,
            &reason_str,
            "sandbox exited",
            sandbox.metadata.generation,
            now_time(),
        ),
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
            applied_cpus: None,
            applied_memory: None,
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
            // The pod is terminating, so any pending resize no longer applies.
            applied_cpus: None,
            applied_memory: None,
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
    Ok(finish())
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
        conditions::ready(
            false,
            "NodeLost",
            "sandbox pod disappeared",
            sandbox.metadata.generation,
            now_time(),
        ),
    );

    let status = SandboxStatus {
        phase: Some(if retrying {
            SandboxPhase::Pending
        } else {
            SandboxPhase::Failed
        }),
        pod_name: if retrying {
            None
        } else {
            prior.and_then(|s| s.pod_name.clone())
        },
        service_name: prior.and_then(|s| s.service_name.clone()),
        node_name: prior.and_then(|s| s.node_name.clone()),
        started_at: if retrying {
            None
        } else {
            prior.and_then(|s| s.started_at.clone())
        },
        terminated_at: Some(now_time()),
        termination_reason: Some(reason),
        exit_code: None,
        restart_count: if retrying {
            restart_count + 1
        } else {
            restart_count
        },
        exposed_ports: prior.map(|s| s.exposed_ports.clone()).unwrap_or_default(),
        applied_cpus: if retrying {
            None
        } else {
            prior.and_then(|s| s.applied_cpus)
        },
        applied_memory: if retrying {
            None
        } else {
            prior.and_then(|s| s.applied_memory)
        },
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
    Ok(finish())
}

/// True if the sandbox's last recorded phase was `Running` — used to tell a
/// pod that vanished mid-run from one not yet created.
fn was_running(sandbox: &Sandbox) -> bool {
    sandbox.status.as_ref().and_then(|s| s.phase.as_ref()) == Some(&SandboxPhase::Running)
}

fn desired_stopped(sandbox: &Sandbox) -> bool {
    sandbox.spec.desired_state == msb_crd::sandbox::DesiredState::Stopped
}

async fn delete_pod(pods: &Api<Pod>, pod_name: &str, name: &str) -> Result<(), Error> {
    if let Err(source) = pods.delete(pod_name, &DeleteParams::default()).await
        && !is_not_found(&source)
    {
        return Err(Error::DeletePod {
            sandbox: name.to_string(),
            pod: pod_name.to_string(),
            source,
        });
    }
    Ok(())
}

/// Record the resting `Stopped` phase for a desired-Stop. Not a terminal state and
/// not a failure: runPolicy does not fire and `restart_count` is preserved, not
/// bumped. The per-sandbox Service is left in place so identity is stable across a
/// stop/start.
async fn mark_stopped(
    sandbox: &Sandbox,
    ctx: &Context,
    sandboxes: &Api<Sandbox>,
    name: &str,
) -> Result<Action, Error> {
    if sandbox.status.as_ref().and_then(|s| s.phase.as_ref()) == Some(&SandboxPhase::Stopped) {
        return Ok(Action::await_change());
    }

    let mut conditions = prior_conditions(sandbox);
    conditions::set(
        &mut conditions,
        conditions::ready(
            false,
            "Stopped",
            "sandbox is stopped by desiredState",
            sandbox.metadata.generation,
            now_time(),
        ),
    );
    let status = SandboxStatus {
        phase: Some(SandboxPhase::Stopped),
        restart_count: sandbox
            .status
            .as_ref()
            .map(|s| s.restart_count)
            .unwrap_or(0),
        conditions,
        ..Default::default()
    };
    patch_status(sandboxes, name, &status).await?;
    ctx.recorder
        .publish(
            &event(
                EventType::Normal,
                "Stopped",
                "sandbox stopped by desiredState",
            ),
            &sandbox.object_ref(&()),
        )
        .await
        .ok();
    info!(sandbox = %name, "stopped");
    Ok(Action::await_change())
}

/// Settle a Sandbox `Failed` with a `PortConflict` condition and create nothing,
/// so status never advertises a Service the collision can't produce.
async fn mark_port_conflict(
    sandbox: &Sandbox,
    ctx: &Context,
    sandboxes: &Api<Sandbox>,
    name: &str,
    port: u16,
) -> Result<Action, Error> {
    let message = format!(
        "publishedPort {port} collides with a reserved bridge port; choose a different hostPort"
    );
    if sandbox.status.as_ref().and_then(|s| s.termination_reason.as_ref())
        == Some(&TerminationReason::Failed)
        && sandbox.status.as_ref().and_then(|s| s.phase.as_ref()) == Some(&SandboxPhase::Failed)
    {
        return Ok(Action::await_change());
    }

    let mut conditions = prior_conditions(sandbox);
    conditions::set(
        &mut conditions,
        conditions::ready(
            false,
            "PortConflict",
            &message,
            sandbox.metadata.generation,
            now_time(),
        ),
    );
    let status = SandboxStatus {
        phase: Some(SandboxPhase::Failed),
        termination_reason: Some(TerminationReason::Failed),
        conditions,
        ..Default::default()
    };
    patch_status(sandboxes, name, &status).await?;
    ctx.recorder
        .publish(
            &event(EventType::Warning, "PortConflict", &message),
            &sandbox.object_ref(&()),
        )
        .await
        .ok();
    warn!(sandbox = %name, port, "published port collides with a reserved bridge port");
    Ok(Action::await_change())
}

/// Keep the terminal CR so its exit code and reason stay readable; cleanup is the
/// user's, or an opt-in `spec.lifecycle` expiry.
fn finish() -> Action {
    Action::await_change()
}

/// The `shutdownTime` deadline passed: delete the pod, then delete or (default)
/// retain-as-`Expired` the object. Only the wall-clock deadline routes here; a
/// guest exiting on its own does not.
async fn expire(
    sandbox: &Sandbox,
    ctx: &Context,
    sandboxes: &Api<Sandbox>,
    pods: &Api<Pod>,
    name: &str,
) -> Result<Action, Error> {
    delete_pod(pods, &pod::pod_name(name), name).await?;

    if sandbox.spec.lifecycle.shutdown_policy == ShutdownPolicy::Delete {
        if let Err(source) = sandboxes.delete(name, &DeleteParams::default()).await
            && !is_not_found(&source)
        {
            return Err(Error::DeleteSandbox {
                sandbox: name.to_string(),
                source,
            });
        }
        info!(sandbox = %name, "expired; deleted per shutdownPolicy");
        return Ok(Action::await_change());
    }

    // Already recorded Expired; nothing left to do.
    if sandbox
        .status
        .as_ref()
        .and_then(|s| s.termination_reason.as_ref())
        == Some(&TerminationReason::Expired)
    {
        return Ok(Action::await_change());
    }

    let mut conditions = prior_conditions(sandbox);
    conditions::set(
        &mut conditions,
        conditions::ready(
            false,
            "Expired",
            "shutdownTime deadline passed",
            sandbox.metadata.generation,
            now_time(),
        ),
    );
    let prior = sandbox.status.as_ref();
    let status = SandboxStatus {
        phase: Some(SandboxPhase::Succeeded),
        termination_reason: Some(TerminationReason::Expired),
        terminated_at: Some(now_time()),
        started_at: prior.and_then(|s| s.started_at.clone()),
        restart_count: prior.map(|s| s.restart_count).unwrap_or(0),
        conditions,
        ..Default::default()
    };
    patch_status(sandboxes, name, &status).await?;
    ctx.recorder
        .publish(
            &event(EventType::Normal, "Expired", "shutdownTime deadline passed"),
            &sandbox.object_ref(&()),
        )
        .await
        .ok();
    info!(sandbox = %name, "expired; retained per shutdownPolicy");
    Ok(Action::await_change())
}

async fn patch_status(
    sandboxes: &Api<Sandbox>,
    name: &str,
    status: &SandboxStatus,
) -> Result<(), Error> {
    let patch = json!({ "status": status });
    patch_status_merge(sandboxes, name, &patch).await
}

/// A merge patch of a partial status document, for updates (e.g. a resize)
/// that touch only a few fields and must not clobber the rest by omission.
async fn patch_status_merge(
    sandboxes: &Api<Sandbox>,
    name: &str,
    patch: &serde_json::Value,
) -> Result<(), Error> {
    sandboxes
        .patch_status(name, &PatchParams::default(), &Patch::Merge(patch))
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

fn now_time() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}

fn shutdown_deadline(sandbox: &Sandbox) -> Option<Time> {
    sandbox.spec.lifecycle.shutdown_time.clone()
}

fn is_expired(sandbox: &Sandbox) -> bool {
    shutdown_deadline(sandbox).is_some_and(|d| now_time().0 >= d.0)
}

fn requeue_until(deadline: &Time) -> Duration {
    // Floor at 1s: a past deadline yields 0 and would hot-loop the requeue.
    let secs = deadline.0.duration_since(now_time().0).as_secs().max(1);
    Duration::from_secs(secs as u64)
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
                                reason: Some(
                                    if exit_code == 0 { "Completed" } else { "Error" }.into(),
                                ),
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
            (SandboxPhase::Stopped, false),
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
    fn desired_stopped_reads_the_spec_field() {
        let mut sb = sandbox();
        assert!(!desired_stopped(&sb), "default is Running");
        sb.spec.desired_state = msb_crd::sandbox::DesiredState::Stopped;
        assert!(desired_stopped(&sb));
    }

    #[test]
    fn stopped_phase_is_not_a_vanished_pod() {
        // A pod removed by a desired Stop must not be seen as an unclean vanish.
        let mut sb = sandbox();
        sb.status = Some(msb_crd::SandboxStatus {
            phase: Some(SandboxPhase::Stopped),
            ..Default::default()
        });
        assert!(!was_running(&sb));
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
        let started = Time("2026-07-17T10:00:00Z".parse().unwrap());
        let prior = SandboxStatus {
            phase: Some(SandboxPhase::Running),
            pod_name: Some("sandbox-my-sandbox".to_string()),
            node_name: Some("node-1".to_string()),
            started_at: Some(started.clone()),
            ..Default::default()
        };

        let terminated = SandboxStatus {
            phase: Some(SandboxPhase::Succeeded),
            pod_name: Some("sandbox-my-sandbox".to_string()),
            node_name: prior.node_name.clone(),
            started_at: prior.started_at.clone(),
            terminated_at: Some(Time("2026-07-17T10:05:00Z".parse().unwrap())),
            termination_reason: Some(TerminationReason::Completed),
            exit_code: Some(0),
            ..Default::default()
        };

        assert_eq!(terminated.started_at, Some(started));
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
        let retries =
            |policy: RunPolicy, succeeded: bool| !succeeded && policy == RunPolicy::RerunOnFailure;
        assert!(
            retries(RunPolicy::RerunOnFailure, false),
            "unclean + policy"
        );
        assert!(
            !retries(RunPolicy::RerunOnFailure, true),
            "clean exit stops"
        );
        assert!(!retries(RunPolicy::Once, false), "Once never retries");
        assert!(!retries(RunPolicy::Once, true));
    }

    #[test]
    fn is_expired_only_when_a_past_deadline_is_set() {
        let mut sb = sandbox();
        assert!(!is_expired(&sb), "no deadline never expires");

        sb.spec.lifecycle.shutdown_time = Some(Time("2000-01-01T00:00:00Z".parse().unwrap()));
        assert!(is_expired(&sb), "past deadline is expired");

        sb.spec.lifecycle.shutdown_time = Some(Time("2999-01-01T00:00:00Z".parse().unwrap()));
        assert!(!is_expired(&sb), "future deadline is not yet expired");
    }

    #[test]
    fn requeue_until_is_at_least_one_second_for_a_past_deadline() {
        let past = Time("2000-01-01T00:00:00Z".parse().unwrap());
        assert_eq!(requeue_until(&past), Duration::from_secs(1));
    }

    #[test]
    fn running_action_requeues_only_with_a_deadline() {
        let mut sb = sandbox();
        assert_eq!(
            running_action(&sb),
            Action::await_change(),
            "no deadline -> await change"
        );
        sb.spec.lifecycle.shutdown_time = Some(Time("2999-01-01T00:00:00Z".parse().unwrap()));
        assert_ne!(
            running_action(&sb),
            Action::await_change(),
            "deadline -> requeue so expiry fires on time"
        );
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
    fn resize_pending_is_false_with_no_status() {
        assert!(
            !resize_pending(&sandbox()),
            "never booted, nothing applied yet"
        );
    }

    #[test]
    fn resize_pending_is_false_when_spec_matches_applied() {
        let mut sb = sandbox();
        sb.status = Some(SandboxStatus {
            applied_cpus: Some(sb.spec.cpus),
            applied_memory: Some(sb.spec.memory),
            ..Default::default()
        });
        assert!(!resize_pending(&sb));
    }

    #[test]
    fn resize_pending_is_true_when_cpus_or_memory_diverge() {
        let mut sb = sandbox();
        sb.status = Some(SandboxStatus {
            applied_cpus: Some(sb.spec.cpus),
            applied_memory: Some(sb.spec.memory),
            ..Default::default()
        });
        sb.spec.cpus += 1;
        assert!(resize_pending(&sb));

        let mut sb2 = sandbox();
        sb2.status = Some(SandboxStatus {
            applied_cpus: Some(sb2.spec.cpus),
            applied_memory: Some(sb2.spec.memory),
            ..Default::default()
        });
        sb2.spec.memory += 1;
        assert!(resize_pending(&sb2));
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
