//! Per-identity concurrent-session caps for the data half.
//!
//! One slow or abusive client must not exhaust the gateway's connection budget
//! for everyone else. Each exec session acquires a slot keyed by the
//! authenticated identity; the slot is released (RAII) when the session ends.
//! State is a tiny in-memory count map — not a database — because caps are a
//! process-local resource concern, not cluster state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Tracks concurrent exec sessions per identity and enforces a cap.
#[derive(Clone)]
pub struct ConnLimiter {
    max_per_identity: usize,
    counts: Arc<Mutex<HashMap<String, usize>>>,
}

impl ConnLimiter {
    pub fn new(max_per_identity: usize) -> Self {
        Self {
            max_per_identity,
            counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Try to take a slot for `identity`. Returns a guard that releases the slot
    /// on drop, or `None` if the identity is already at its cap.
    pub fn acquire(&self, identity: &str) -> Option<ConnGuard> {
        // A zero (or absurd) cap means "no limit" — never block.
        if self.max_per_identity == 0 {
            return Some(ConnGuard { limiter: None });
        }
        let mut counts = self.counts.lock().expect("conn-limiter mutex");
        let n = counts.entry(identity.to_string()).or_insert(0);
        if *n >= self.max_per_identity {
            return None;
        }
        *n += 1;
        Some(ConnGuard {
            limiter: Some((self.clone(), identity.to_string())),
        })
    }

    fn release(&self, identity: &str) {
        let mut counts = self.counts.lock().expect("conn-limiter mutex");
        if let Some(n) = counts.get_mut(identity) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.remove(identity);
            }
        }
    }
}

/// Releases the acquired slot when dropped. Holds no lock while alive.
pub struct ConnGuard {
    limiter: Option<(ConnLimiter, String)>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if let Some((limiter, identity)) = self.limiter.take() {
            limiter.release(&identity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_per_identity_and_releases_on_drop() {
        let lim = ConnLimiter::new(2);
        let g1 = lim.acquire("alice");
        let g2 = lim.acquire("alice");
        assert!(g1.is_some() && g2.is_some());
        // Third concurrent session for alice is refused.
        assert!(lim.acquire("alice").is_none());
        // A different identity is unaffected.
        assert!(lim.acquire("bob").is_some());
        // Dropping one frees a slot.
        drop(g1);
        assert!(lim.acquire("alice").is_some());
    }

    #[test]
    fn zero_means_unlimited() {
        let lim = ConnLimiter::new(0);
        let mut guards = Vec::new();
        for _ in 0..100 {
            guards.push(lim.acquire("x").expect("unlimited"));
        }
    }

    #[test]
    fn count_map_cleaned_up_when_identity_drops_to_zero() {
        let lim = ConnLimiter::new(4);
        {
            let _g = lim.acquire("ephemeral").unwrap();
            assert_eq!(lim.counts.lock().unwrap().get("ephemeral"), Some(&1));
        }
        // After the guard drops, the entry is gone (no unbounded growth).
        assert!(lim.counts.lock().unwrap().get("ephemeral").is_none());
    }
}
