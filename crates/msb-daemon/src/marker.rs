//! Writes the cache-ready markers the runtime waits on (paths from `msb_crd::cache`).

use std::io::Write;
use std::path::Path;

use msb_crd::cache;
use tracing::warn;

pub fn write_ready(cache_root: &Path, image: &str) {
    write_atomic(&cache::ready_marker(cache_root, image), b"");
}

pub fn write_failed(cache_root: &Path, image: &str, reason: &str) {
    write_atomic(&cache::failed_marker(cache_root, image), reason.as_bytes());
}

/// Write-temp-then-rename so a reader on the shared mount never sees a partial
/// marker. Best-effort: a write failure is logged, and the runtime's wait times out.
fn write_atomic(path: &Path, contents: &[u8]) {
    let Some(dir) = path.parent() else { return };
    if let Err(e) = std::fs::create_dir_all(dir) {
        warn!(dir = %dir.display(), error = %e, "creating marker dir");
        return;
    }
    let tmp = path.with_extension("tmp");
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        warn!(marker = %path.display(), error = %e, "writing marker");
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_marker_is_created_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        write_ready(dir.path(), "python:3.12");
        assert!(cache::ready_marker(dir.path(), "python:3.12").exists());
    }

    #[test]
    fn failed_marker_carries_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        write_failed(dir.path(), "bad:ref", "auth denied");
        let body = std::fs::read_to_string(cache::failed_marker(dir.path(), "bad:ref")).unwrap();
        assert_eq!(body, "auth denied");
    }
}
