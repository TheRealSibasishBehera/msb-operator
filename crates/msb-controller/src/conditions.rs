//! Status conditions, mirroring `meta.SetStatusCondition` semantics: an upsert
//! keyed on `type` that preserves `lastTransitionTime` when only the message or
//! reason changes, and stamps a fresh one only when `status` flips.

use msb_crd::SandboxCondition;

pub const READY: &str = "Ready";

/// Upsert by `type`, keeping the prior `last_transition_time` unless `status`
/// changed (see module docs).
pub fn set(conditions: &mut Vec<SandboxCondition>, mut condition: SandboxCondition) {
    if let Some(existing) = conditions.iter_mut().find(|c| c.type_ == condition.type_) {
        if existing.status == condition.status {
            condition.last_transition_time = existing.last_transition_time.clone();
        }
        *existing = condition;
    } else {
        conditions.push(condition);
    }
}

pub fn ready(status: bool, reason: &str, message: &str, now: String) -> SandboxCondition {
    SandboxCondition {
        type_: READY.to_string(),
        status: if status { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(status: &str, ts: &str) -> SandboxCondition {
        SandboxCondition {
            type_: READY.to_string(),
            status: status.to_string(),
            reason: "R".to_string(),
            message: "m".to_string(),
            last_transition_time: ts.to_string(),
        }
    }

    #[test]
    fn insert_then_update_in_place_by_type() {
        let mut v = Vec::new();
        set(&mut v, cond("True", "t1"));
        set(&mut v, cond("True", "t2"));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn same_status_keeps_original_transition_time() {
        let mut v = vec![cond("True", "t1")];
        set(&mut v, cond("True", "t2"));
        assert_eq!(v[0].last_transition_time, "t1");
    }

    #[test]
    fn status_flip_stamps_new_transition_time() {
        let mut v = vec![cond("True", "t1")];
        set(&mut v, cond("False", "t2"));
        assert_eq!(v[0].last_transition_time, "t2");
        assert_eq!(v[0].status, "False");
    }
}
