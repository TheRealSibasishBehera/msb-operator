use std::fmt;

/// Longest `MSB_HOME` that keeps the hashed agent socket path inside `sun_path`.
///
/// The socket is `$MSB_HOME/run/agent/<32 hex>.sock`, costing a fixed 48 bytes
/// beyond the home path, and msb requires the total to be under 108 on Linux.
/// Past this limit msb silently falls back to a legacy socket layout, and the
/// bridge — which dials the hashed path — cannot connect.
pub const MSB_HOME_MAX_BYTES: usize = 59;

pub const KVM_RESOURCE: &str = "devices.microsandbox.io/kvm";
pub const SANDBOX_LABEL: &str = "microsandbox.io/sandbox";
pub const FIELD_MANAGER: &str = "msb-controller";

/// Must be `$MSB_HOME/cache` so the cache's baked absolute VMDK paths resolve.
pub const CACHE_MOUNT: &str = "/msb/cache";

// Runtime-container resource sizing. On top of the guest RAM the pod carries the
// VMM + runtime process overhead. The base is calibrated from a measured idle
// boot (runtime + VMM peak ≈ 75Mi at 512Mi/1vCPU), rounded up for headroom.
pub const RUNTIME_BASE_OVERHEAD_MIB: u64 = 96;
pub const PER_VCPU_OVERHEAD_MIB: u64 = 8;
/// Shared-CPU allocation ratio: cpu request = vCPUs/ratio.
pub const CPU_ALLOCATION_RATIO: u64 = 10;
pub const EPHEMERAL_STORAGE_MIB: u64 = 50;

/// Overhead added to guest RAM. The `guest/512` term is the page-table cost
/// (one bit per 512 bytes of RAM); the rest is the process base and per-vCPU
/// structures.
pub fn memory_overhead_mib(guest_mib: u64, vcpus: u64) -> u64 {
    RUNTIME_BASE_OVERHEAD_MIB + guest_mib / 512 + PER_VCPU_OVERHEAD_MIB * vcpus
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "msbHome {path:?} is {len} bytes; must be <= {MSB_HOME_MAX_BYTES} or msb falls back to \
         the legacy agent socket path and the bridge cannot connect"
    )]
    MsbHomeTooLong { path: String, len: usize },

    #[error("msbHome {0:?} must be an absolute path")]
    MsbHomeNotAbsolute(String),
}

/// Node-dependent settings supplied by the Helm chart.
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    msb_home: String,
    /// GID of `/dev/kvm` on the node. Not standardised — Ubuntu assigns it
    /// dynamically, so it is a deployment-time value, not a constant.
    pub kvm_gid: i64,
    pub runtime_image: String,
    pub bridge_image: String,
    pub bridge_port: i32,
    /// Registry/repo prefix for pre-baked cache images. The controller derives a
    /// sandbox's cache-image reference as `<prefix>/<slug>-<hash>` from
    /// `spec.image`. Empty means no prefix (bare derived name — for local tags).
    pub cache_prefix: String,
}

impl ControllerConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        msb_home: impl Into<String>,
        kvm_gid: i64,
        runtime_image: impl Into<String>,
        bridge_image: impl Into<String>,
        bridge_port: i32,
        cache_prefix: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        let msb_home = msb_home.into();

        if !msb_home.starts_with('/') {
            return Err(ConfigError::MsbHomeNotAbsolute(msb_home));
        }
        if msb_home.len() > MSB_HOME_MAX_BYTES {
            return Err(ConfigError::MsbHomeTooLong {
                len: msb_home.len(),
                path: msb_home,
            });
        }

        Ok(Self {
            msb_home,
            kvm_gid,
            runtime_image: runtime_image.into(),
            bridge_image: bridge_image.into(),
            bridge_port,
            cache_prefix: cache_prefix.into(),
        })
    }

    /// Validated at construction, so it cannot be set past `MSB_HOME_MAX_BYTES`.
    pub fn msb_home(&self) -> &str {
        &self.msb_home
    }

    /// The pre-baked cache image reference for a given app image. `spec.image`
    /// is the app ref msb keys the cache by; this is where the kubelet pulls the
    /// cache from. See [`derive_cache_ref`].
    pub fn cache_ref(&self, app_image: &str) -> String {
        derive_cache_ref(&self.cache_prefix, app_image)
    }
}

/// Maps an app image reference to its pre-baked cache image reference:
/// `<prefix>/<slug>-<hash>`, where `slug` is the last path segment plus tag
/// (sanitized to `[a-z0-9-]`, capped) and `hash` is the first 12 hex of
/// `sha256(app_image)`. The hash makes it collision-free; the slug keeps it
/// legible. **The cache-build tooling must compute this identically** — the
/// algorithm is the contract between the two.
pub fn derive_cache_ref(prefix: &str, app_image: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(app_image.as_bytes()));
    let short = &hash[..12];

    // Last path segment (drop registry/repo), then normalize `:`/`@` to `-`.
    let last = app_image.rsplit('/').next().unwrap_or(app_image);
    let mut slug: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of `-`, trim, and cap so the tag stays a sane length.
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(40).collect();

    let name = format!("{slug}-{short}");
    if prefix.is_empty() {
        name
    } else {
        format!("{}/{name}", prefix.trim_end_matches('/'))
    }
}

impl fmt::Display for ControllerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "msb_home={} kvm_gid={}", self.msb_home, self.kvm_gid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_home(home: &str) -> Result<ControllerConfig, ConfigError> {
        ControllerConfig::new(
            home,
            104,
            "runtime:dev",
            "bridge:dev",
            7000,
            "registry.example.com/msb-cache",
        )
    }

    #[test]
    fn derives_cache_ref_with_slug_and_hash() {
        let r = derive_cache_ref("reg/msb-cache", "alpine:3.20");
        // <prefix>/<slug>-<12 hex>; slug from last segment, `:` -> `-`.
        assert!(r.starts_with("reg/msb-cache/alpine-3-20-"), "{r}");
        let hash = r.rsplit('-').next().unwrap();
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cache_ref_drops_registry_and_repo_from_slug() {
        let r = derive_cache_ref("p", "ghcr.io/foo/bar:v1");
        assert!(r.starts_with("p/bar-v1-"), "{r}");
    }

    #[test]
    fn cache_ref_is_collision_free_across_similar_refs() {
        // Different full refs must not share a cache ref even if slugs match.
        let a = derive_cache_ref("p", "foo/bar:1");
        let b = derive_cache_ref("p", "baz/bar:1");
        assert_ne!(a, b);
    }

    #[test]
    fn empty_prefix_yields_bare_derived_name() {
        let r = derive_cache_ref("", "alpine:3.20");
        assert!(r.starts_with("alpine-3-20-"), "{r}");
        assert!(!r.contains('/'), "{r}");
    }

    #[test]
    fn cache_ref_matches_the_build_tooling_golden_values() {
        // Pins the exact output so drift from docker/cache-image/build.sh's
        // derive_cache_ref (which must stay identical) is caught.
        assert_eq!(
            derive_cache_ref("reg/msb-cache", "alpine:3.20"),
            "reg/msb-cache/alpine-3-20-d3dfed77bb64"
        );
        assert_eq!(
            derive_cache_ref("reg/msb-cache", "python:3.12"),
            "reg/msb-cache/python-3-12-e3efd51c9a35"
        );
        assert_eq!(
            derive_cache_ref("reg/msb-cache", "ghcr.io/foo/bar:v1"),
            "reg/msb-cache/bar-v1-02f3f049157a"
        );
    }

    #[test]
    fn accepts_a_short_absolute_home() {
        let cfg = config_with_home("/var/lib/msb").expect("valid home");
        assert_eq!(cfg.msb_home(), "/var/lib/msb");
    }

    #[test]
    fn accepts_home_at_exactly_the_limit() {
        let home = format!("/{}", "a".repeat(MSB_HOME_MAX_BYTES - 1));
        assert_eq!(home.len(), MSB_HOME_MAX_BYTES);
        assert!(config_with_home(&home).is_ok());
    }

    #[test]
    fn rejects_home_one_byte_over_the_limit() {
        let home = format!("/{}", "a".repeat(MSB_HOME_MAX_BYTES));
        assert_eq!(home.len(), MSB_HOME_MAX_BYTES + 1);
        assert!(matches!(
            config_with_home(&home),
            Err(ConfigError::MsbHomeTooLong { .. })
        ));
    }

    #[test]
    fn rejects_relative_home() {
        assert!(matches!(
            config_with_home("var/lib/msb"),
            Err(ConfigError::MsbHomeNotAbsolute(_))
        ));
    }
}
