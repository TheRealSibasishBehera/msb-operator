//! Status conditions, using the standard `metav1.Condition`. `set` mirrors
//! `meta.SetStatusCondition`: an upsert keyed on `type` that keeps
//! `lastTransitionTime` unless `status` flips, and stamps `observedGeneration`
//! from the object under reconcile.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

pub const READY: &str = "Ready";
pub const RESTART_REQUIRED: &str = "RestartRequired";

/// Upsert by `type`, keeping the prior `lastTransitionTime` unless `status`
/// changed.
pub fn set(conditions: &mut Vec<Condition>, mut condition: Condition) {
    if let Some(existing) = conditions.iter_mut().find(|c| c.type_ == condition.type_) {
        if existing.status == condition.status {
            condition.last_transition_time = existing.last_transition_time.clone();
        }
        *existing = condition;
    } else {
        conditions.push(condition);
    }
}

fn condition(
    type_: &str,
    status: bool,
    reason: &str,
    message: &str,
    generation: Option<i64>,
    now: Time,
) -> Condition {
    Condition {
        type_: type_.to_string(),
        status: if status { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: now,
        observed_generation: generation,
    }
}

pub fn ready(
    status: bool,
    reason: &str,
    message: &str,
    generation: Option<i64>,
    now: Time,
) -> Condition {
    condition(READY, status, reason, message, generation, now)
}

/// A `spec.cpus`/`spec.memory` edit that could not be applied live: the sandbox
/// booted without hotplug headroom, or the control socket was unreachable. The
/// controller never auto-restarts to apply it (a restart on the current storage
/// model loses guest state); the user restarts explicitly.
pub fn restart_required(
    status: bool,
    reason: &str,
    message: &str,
    generation: Option<i64>,
    now: Time,
) -> Condition {
    condition(RESTART_REQUIRED, status, reason, message, generation, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(status: &str, ts: &str) -> Condition {
        Condition {
            type_: READY.to_string(),
            status: status.to_string(),
            reason: "R".to_string(),
            message: "m".to_string(),
            last_transition_time: Time(ts.parse().unwrap()),
            observed_generation: None,
        }
    }

    fn ts(s: &str) -> Time {
        Time(s.parse().unwrap())
    }

    #[test]
    fn insert_then_update_in_place_by_type() {
        let mut v = Vec::new();
        set(&mut v, cond("True", "2026-01-01T00:00:00Z"));
        set(&mut v, cond("True", "2026-01-02T00:00:00Z"));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn same_status_keeps_original_transition_time() {
        let mut v = vec![cond("True", "2026-01-01T00:00:00Z")];
        set(&mut v, cond("True", "2026-01-02T00:00:00Z"));
        assert_eq!(v[0].last_transition_time, ts("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn status_flip_stamps_new_transition_time() {
        let mut v = vec![cond("True", "2026-01-01T00:00:00Z")];
        set(&mut v, cond("False", "2026-01-02T00:00:00Z"));
        assert_eq!(v[0].last_transition_time, ts("2026-01-02T00:00:00Z"));
        assert_eq!(v[0].status, "False");
    }
}
