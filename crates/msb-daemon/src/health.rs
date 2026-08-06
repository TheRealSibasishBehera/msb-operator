use std::path::PathBuf;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::watch;
use tracing::info;

pub async fn watch_path(path: PathBuf) -> anyhow::Result<watch::Receiver<bool>> {
    let exists = path.exists();
    let (tx, rx) = watch::channel(exists);

    // Canonicalize the parent so that event paths from notify (which are
    // canonical) match correctly on macOS where tempdir may be a symlink.
    let parent = path
        .parent()
        .map(|p| p.to_owned())
        .unwrap_or_else(|| PathBuf::from("/"));
    let parent_canonical = parent.canonicalize().unwrap_or(parent);
    let watch_path_canonical = parent_canonical.join(
        path.file_name()
            .expect("path must have a file name component"),
    );

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);

    let mut watcher = RecommendedWatcher::new(
        move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result {
                let _ = event_tx.blocking_send(event);
            }
        },
        notify::Config::default(),
    )?;

    watcher.watch(&parent_canonical, RecursiveMode::NonRecursive)?;

    tokio::spawn(async move {
        let _watcher = watcher;

        while let Some(event) = event_rx.recv().await {
            let touches_target = event.paths.iter().any(|p| p == &watch_path_canonical);

            if !touches_target {
                continue;
            }

            let relevant = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)
            );
            if !relevant {
                continue;
            }

            let now_exists = watch_path_canonical.exists();
            let prev = *tx.borrow();
            if now_exists != prev {
                info!(
                    path = %watch_path_canonical.display(),
                    healthy = now_exists,
                    "device health changed"
                );
                let _ = tx.send(now_exists);
            }
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    #[tokio::test]
    async fn detects_create_and_delete() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("fake-kvm");

        let mut rx = watch_path(path.clone()).await.unwrap();
        assert!(!*rx.borrow(), "should start unhealthy when absent");

        fs::write(&path, b"").unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("timed out waiting for healthy signal")
            .unwrap();
        assert!(*rx.borrow(), "should become healthy after create");

        fs::remove_file(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("timed out waiting for unhealthy signal")
            .unwrap();
        assert!(!*rx.borrow(), "should become unhealthy after delete");
    }
}
