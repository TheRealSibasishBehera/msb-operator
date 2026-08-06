//! Cache-ready marker paths, shared so the daemon (writer) and runtime (reader)
//! agree. Keyed on `sha256(spec.image)` — the one string both sides hold.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const READY_DIR: &str = "ready";

pub fn marker_key(image: &str) -> String {
    hex::encode(Sha256::digest(image.as_bytes()))
}

pub fn ready_marker(cache_root: &Path, image: &str) -> PathBuf {
    cache_root.join(READY_DIR).join(marker_key(image))
}

pub fn failed_marker(cache_root: &Path, image: &str) -> PathBuf {
    cache_root
        .join(READY_DIR)
        .join(format!("{}.failed", marker_key(image)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_hex_sha256() {
        let k = marker_key("python:3.12");
        assert_eq!(k.len(), 64);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(k, marker_key("python:3.12"));
        assert_ne!(k, marker_key("python:3.13"));
    }

    #[test]
    fn markers_sit_under_the_ready_dir() {
        let root = Path::new("/msb/cache");
        let ready = ready_marker(root, "alpine:3.20");
        let failed = failed_marker(root, "alpine:3.20");
        assert!(ready.starts_with("/msb/cache/ready/"));
        assert_eq!(
            failed.file_name().unwrap().to_str().unwrap(),
            format!("{}.failed", marker_key("alpine:3.20"))
        );
    }
}
