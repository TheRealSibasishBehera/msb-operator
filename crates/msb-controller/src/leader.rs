use std::time::Duration;

use kube::Client;
use kube_leader_election::{LeaseLock, LeaseLockParams, LeaseLockResult};
use tokio::time::interval;
use tracing::{info, warn};

const LEASE_NAME: &str = "msb-controller";

/// Runs `work` only while this replica holds the leader lease, renewing in the
/// background and stepping down cleanly on cancellation.
///
/// TTL/renew ratio is 3:1 (renew at a third of the lease lifetime), the
/// conventional margin so a transient renewal failure does not drop leadership.
///
/// On lease loss this returns `Ok(())` and the process exits: the Deployment
/// restarts it as a fresh standby that re-contends, rather than looping back
/// internally. Simpler, and matches how a crashed leader recovers anyway.
///
/// Known limitation: between a leader losing its lease and a standby acquiring
/// the expired one, there is a bounded window (< lease TTL) where both could
/// reconcile the same Sandbox. Pod writes carry no fencing token, so a stale
/// leader's in-flight write can race the new leader's. The 3:1 margin keeps the
/// window small; true fencing (resourceVersion preconditions on pod writes) is
/// deferred — the failure mode is a transient pod flap, not data loss.
pub async fn run_when_leader<F, Fut>(
    client: Client,
    namespace: &str,
    holder_id: &str,
    lease_ttl: Duration,
    work: F,
) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let lock = LeaseLock::new(
        client,
        namespace,
        LeaseLockParams {
            lease_name: LEASE_NAME.to_string(),
            holder_id: holder_id.to_string(),
            lease_ttl,
        },
    );

    let renew_every = lease_ttl / 3;

    info!(holder = %holder_id, "waiting for leadership");
    // A standby blocks here; SIGTERM during a rolling update must still exit it
    // cleanly, since this path no longer goes through Controller::shutdown_on_signal.
    let outcome = tokio::select! {
        acquired = wait_until_acquired(&lock, renew_every) => {
            acquired?;
            info!(holder = %holder_id, "acquired leadership");
            tokio::select! {
                _ = work() => Outcome::WorkFinished,
                _ = renew_loop(&lock, renew_every) => Outcome::LostLease,
            }
        }
        _ = shutdown_signal() => Outcome::ShutdownWhileStandby,
    };

    match outcome {
        Outcome::LostLease => warn!("lost leadership; stepping down"),
        Outcome::ShutdownWhileStandby => {
            info!("shutdown signal while standby; exiting");
        }
        Outcome::WorkFinished => {}
    }

    // Best-effort clean handoff so a standby takes over immediately rather than
    // waiting for the TTL to expire.
    if let Err(error) = lock.step_down().await {
        warn!(%error, "step-down failed; standby will wait for TTL expiry");
    }

    Ok(())
}

enum Outcome {
    WorkFinished,
    LostLease,
    ShutdownWhileStandby,
}

/// Resolves on SIGTERM (rolling update / scale-down) or SIGINT (Ctrl-C).
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(error) => {
            warn!(%error, "cannot install SIGTERM handler; standby won't exit on signal");
            std::future::pending::<()>().await;
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => {
            term.recv().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

async fn wait_until_acquired(lock: &LeaseLock, poll: Duration) -> anyhow::Result<()> {
    // A standby legitimately sees NotAcquired indefinitely, so only *errors*
    // are capped. Persistent errors mean a real misconfiguration (bad RBAC,
    // unreachable apiserver) that should crash the pod to surface it, not spin
    // silently doing no work.
    const MAX_ACQUIRE_ERRORS: u32 = 5;

    let mut tick = interval(poll);
    let mut consecutive_errors = 0u32;
    loop {
        tick.tick().await;
        match lock.try_acquire_or_renew().await {
            Ok(LeaseLockResult::Acquired(_)) => return Ok(()),
            Ok(LeaseLockResult::NotAcquired(_)) => consecutive_errors = 0,
            Err(error) => {
                consecutive_errors += 1;
                warn!(%error, consecutive_errors, "lease acquire failed");
                if consecutive_errors >= MAX_ACQUIRE_ERRORS {
                    return Err(anyhow::anyhow!(
                        "gave up acquiring leadership after {MAX_ACQUIRE_ERRORS} \
                         consecutive errors: {error}"
                    ));
                }
            }
        }
    }
}

/// Renews on each tick; returns to signal leadership loss to the caller.
///
/// `NotAcquired` is a definitive loss (someone else holds it) and returns at
/// once. An API error is *not* proof of loss — the 3:1 renew margin means a
/// single blip leaves the lease valid — so errors are tolerated up to
/// `MAX_RENEW_ERRORS` consecutive failures before we assume the apiserver is
/// unreachable and step down. A success resets the counter.
async fn renew_loop(lock: &LeaseLock, renew_every: Duration) {
    const MAX_RENEW_ERRORS: u32 = 2;

    let mut tick = interval(renew_every);
    let mut consecutive_errors = 0u32;
    loop {
        tick.tick().await;
        match lock.try_acquire_or_renew().await {
            Ok(LeaseLockResult::Acquired(_)) => consecutive_errors = 0,
            Ok(LeaseLockResult::NotAcquired(_)) => {
                warn!("lease taken by another holder; stepping down");
                return;
            }
            Err(error) => {
                consecutive_errors += 1;
                warn!(%error, consecutive_errors, "lease renewal failed");
                if consecutive_errors >= MAX_RENEW_ERRORS {
                    warn!("renewal failed repeatedly; assuming leadership lost");
                    return;
                }
            }
        }
    }
}
