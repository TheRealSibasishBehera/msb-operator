use std::fmt;

/// Longest `MSB_HOME` that keeps the hashed agent socket path inside `sun_path`.
///
/// The socket is `$MSB_HOME/run/agent/<32 hex>.sock`, costing a fixed 48 bytes
/// beyond the home path, and msb requires the total to be under 108 on Linux.
/// Past this limit msb silently falls back to a legacy socket layout, and the
/// bridge — which dials the hashed path — cannot connect.
pub const MSB_HOME_MAX_BYTES: usize = 59;

pub const KVM_RESOURCE: &str = "devices.microsandbox.dev/kvm";
pub const SANDBOX_LABEL: &str = "microsandbox.dev/sandbox";
pub const FIELD_MANAGER: &str = "msb-controller";

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
    pub runtime_image: String,
    pub bridge_image: String,
    pub bridge_port: i32,
    /// The bridge's health/control HTTP port; carries both `/healthz` and the
    /// `/control` resize-relay route.
    pub bridge_control_port: i32,
    /// `RUST_LOG` stamped onto the runtime container so its boot is observable.
    pub runtime_log: String,
}

impl ControllerConfig {
    pub fn new(
        msb_home: impl Into<String>,
        runtime_image: impl Into<String>,
        bridge_image: impl Into<String>,
        bridge_port: i32,
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
            runtime_image: runtime_image.into(),
            bridge_image: bridge_image.into(),
            bridge_port,
            bridge_control_port: 8080,
            runtime_log: "info".to_string(),
        })
    }

    /// Sets the bridge's health/control HTTP port. Defaults to 8080, matching
    /// the bridge's own `MSB_BRIDGE_HEALTH_PORT` default.
    pub fn with_bridge_control_port(mut self, port: i32) -> Self {
        self.bridge_control_port = port;
        self
    }

    /// Sets the `RUST_LOG` the runtime container runs with.
    pub fn with_runtime_log(mut self, level: impl Into<String>) -> Self {
        self.runtime_log = level.into();
        self
    }

    /// Validated at construction, so it cannot be set past `MSB_HOME_MAX_BYTES`.
    pub fn msb_home(&self) -> &str {
        &self.msb_home
    }
}

impl fmt::Display for ControllerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "msb_home={}", self.msb_home)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_home(home: &str) -> Result<ControllerConfig, ConfigError> {
        ControllerConfig::new(home, "runtime:dev", "bridge:dev", 7000)
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
