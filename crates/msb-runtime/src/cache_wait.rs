//! Waits for the daemon's cache-ready marker before boot (boot is
//! `PullPolicy::Never`, so the cache must exist first).

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use msb_crd::cache;
use tracing::info;

/// Cold pull+convert budget before failing the pod rather than hanging it.
const DEADLINE: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_millis(250);

/// Ok on the ready marker; errors on the failure marker or the deadline.
pub async fn wait(cache_root: &Path, image: &str) -> Result<()> {
    let ready = cache::ready_marker(cache_root, image);
    let failed = cache::failed_marker(cache_root, image);

    let start = std::time::Instant::now();
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
        tokio::time::sleep(POLL).await;
    }
}
