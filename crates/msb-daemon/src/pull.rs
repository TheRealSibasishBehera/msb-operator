//! Pull and convert an image into the node cache via `msb pull` (the SDK has no
//! pull-only entrypoint).

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;
use tracing::info;

#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error("spawning `msb pull {image}`: {source}")]
    Spawn {
        image: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`msb pull {image}` exited {code}: {stderr}")]
    Exit {
        image: String,
        code: String,
        stderr: String,
    },

    #[error("`msb pull {image}` left no VMDK under {cache}")]
    NoArtifacts { image: String, cache: String },
}

/// `msb_home` must be the path the sandbox mounts the cache at (`/msb`), not the
/// host backing dir, so the VMDK's baked extents resolve. Idempotent.
pub async fn pull(msb: &Path, msb_home: &Path, image: &str) -> Result<(), PullError> {
    info!(%image, msb_home = %msb_home.display(), "pulling + converting image");

    let output = Command::new(msb)
        .arg("pull")
        .arg(image)
        .env("MSB_HOME", msb_home)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| PullError::Spawn {
            image: image.to_string(),
            source,
        })?;

    if !output.status.success() {
        return Err(PullError::Exit {
            image: image.to_string(),
            code: output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    // Assert the VMDK landed: a silent no-op must not pass as a populated cache.
    let vmdk_dir = msb_home.join("cache/vmdk");
    let has_vmdk = std::fs::read_dir(&vmdk_dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == "vmdk"));
    if !has_vmdk {
        return Err(PullError::NoArtifacts {
            image: image.to_string(),
            cache: msb_home.join("cache").display().to_string(),
        });
    }

    info!(%image, "cache ready");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_failure_when_msb_is_missing() {
        let err = pull(
            Path::new("/nonexistent/msb"),
            Path::new("/msb"),
            "python:3.12",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PullError::Spawn { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported() {
        let false_bin = ["/usr/bin/false", "/bin/false"]
            .into_iter()
            .map(Path::new)
            .find(|p| p.exists())
            .expect("a `false` binary");
        let err = pull(false_bin, Path::new("/msb"), "python:3.12")
            .await
            .unwrap_err();
        assert!(matches!(err, PullError::Exit { .. }), "got {err:?}");
    }
}
