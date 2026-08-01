//! Resolving and dialing the sandbox's agent relay socket.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UnixStream;

/// The relay socket path for a sandbox: `$MSB_HOME/run/agent/<hash>.sock`, where
/// `<hash>` is the first 16 bytes of `sha256(name)` as hex — the same scheme msb
/// uses to create it, so we dial exactly the path it writes.
pub fn socket_path(msb_home: &Path, sandbox_name: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(sandbox_name.as_bytes());
    let hash = hex::encode(&digest[..16]);
    msb_home
        .join("run")
        .join("agent")
        .join(format!("{hash}.sock"))
}

/// The control socket path for a given agent socket path: `<sandbox>.sock`
/// becomes `<sandbox>.control.sock`, msb's own derivation.
pub fn control_socket_path(agent_sock: &Path) -> PathBuf {
    agent_sock.with_extension("control.sock")
}

/// Dials the socket, retrying until it appears (msb creates it shortly after
/// boot).
pub async fn dial(path: &Path, retry_for: Duration) -> Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + retry_for;
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("dialing agent socket {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_matches_msb_scheme() {
        // sha256("s")[..16] as hex, under $MSB_HOME/run/agent/.
        let p = socket_path(Path::new("/msb"), "s");
        let s = p.to_str().unwrap();
        assert!(s.starts_with("/msb/run/agent/"));
        assert!(s.ends_with(".sock"));
        let hash = s
            .strip_prefix("/msb/run/agent/")
            .unwrap()
            .strip_suffix(".sock")
            .unwrap();
        assert_eq!(hash.len(), 32);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn control_socket_path_replaces_the_extension() {
        let agent = Path::new("/msb/run/agent/abc.sock");
        assert_eq!(
            control_socket_path(agent),
            Path::new("/msb/run/agent/abc.control.sock")
        );
    }
}
