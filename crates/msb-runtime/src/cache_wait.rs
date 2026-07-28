//! Waits for the daemon's cache-ready marker before boot (boot is
//! `PullPolicy::Never`, so the cache must exist first).

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use msb_crd::cache;
use tracing::info;

/// Cold pull+convert budget before failing the pod rather than hanging it.
const DEADLINE: Duration = Duration::from_secs(300);

// Tight at first (warm returns instantly), doubling to a cap for a long pull.
const POLL_MIN: Duration = Duration::from_millis(20);
const POLL_MAX: Duration = Duration::from_secs(3);

/// Ok on the ready marker; errors on the failure marker or the deadline.
pub async fn wait(cache_root: &Path, image: &str) -> Result<()> {
    let ready = cache::ready_marker(cache_root, image);
    let failed = cache::failed_marker(cache_root, image);

    let start = std::time::Instant::now();
    let mut poll = POLL_MIN;
    let mut logged_wait = false;
    loop {
        if ready.exists() {
            return Ok(());
        }
        if let Ok(reason) = std::fs::read_to_string(&failed) {
            bail!("daemon failed to pull {image}: {reason}");
        }
        if start.elapsed() >= DEADLINE {
            bail!("cache for {image} not ready after {DEADLINE:?}");
        }
        if !logged_wait {
            info!(%image, "waiting for the node cache");
            logged_wait = true;
        }
        tokio::time::sleep(poll).await;
        poll = (poll * 2).min(POLL_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_ok_when_the_ready_marker_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let ready = cache::ready_marker(dir.path(), "python:3.12");
        std::fs::create_dir_all(ready.parent().unwrap()).unwrap();
        std::fs::write(&ready, b"").unwrap();
        assert!(wait(dir.path(), "python:3.12").await.is_ok());
    }

    #[tokio::test]
    async fn fails_fast_on_the_failure_marker() {
        let dir = tempfile::tempdir().unwrap();
        let failed = cache::failed_marker(dir.path(), "bad:ref");
        std::fs::create_dir_all(failed.parent().unwrap()).unwrap();
        std::fs::write(&failed, b"auth denied").unwrap();
        let err = wait(dir.path(), "bad:ref").await.unwrap_err().to_string();
        assert!(err.contains("auth denied"), "{err}");
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut poll = POLL_MIN;
        for _ in 0..20 {
            poll = (poll * 2).min(POLL_MAX);
        }
        assert_eq!(poll, POLL_MAX, "backoff must settle at the cap");
    }
}
