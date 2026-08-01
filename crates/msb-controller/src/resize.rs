//! Applying a `spec.cpus`/`spec.memory` edit to a running sandbox: live, over
//! the bridge's `/control` relay, when the new value fits inside the booted
//! max envelope, otherwise `RestartRequired`.

use msb_crd::SandboxSpec;

/// Whether hotplug headroom may have been reserved at boot for this dimension.
/// `maxCpus`/`maxMemory` unset means the VM booted with no hotplug capacity and no
/// control listener, so a change needs a restart — skip the doomed attempt. When
/// set, the control socket is the authority: a change it can't honor comes back
/// `ok:false` and is classified as restart-required by the caller. `max_*` are
/// immutable, so this answer is stable for the sandbox's life.
pub fn cpus_have_headroom(spec: &SandboxSpec) -> bool {
    spec.max_cpus.is_some()
}

pub fn memory_has_headroom(spec: &SandboxSpec) -> bool {
    spec.max_memory.is_some()
}

/// The control socket's per-op request/response pair, both raw JSON: the
/// controller does not deserialize `ControlResponse`, only checks `"ok":true`.
#[derive(Debug, thiserror::Error)]
pub enum ResizeError {
    #[error("POSTing to {url}: {source}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("bridge returned {status} from {url}: {body}")]
    BridgeStatus {
        url: String,
        status: reqwest::StatusCode,
        body: String,
    },

    #[error("control socket rejected the request: {0}")]
    ControlRejected(String),

    #[error("parsing the control reply: {0}")]
    InvalidReply(#[source] serde_json::Error),
}

/// Base URL for a sandbox's bridge control endpoint, reached through the
/// per-sandbox Service (never the pod IP directly — the Service is the stable
/// address across a resize, and its DNS resolves inside the cluster only).
pub fn control_url(service_name: &str, namespace: &str, control_port: i32) -> String {
    format!("http://{service_name}.{namespace}.svc:{control_port}/control")
}

/// POSTs one control request line and returns the parsed reply, erroring on a
/// non-2xx bridge response or an `"ok":false` control reply.
pub async fn apply(
    client: &reqwest::Client,
    url: &str,
    request: &serde_json::Value,
) -> Result<serde_json::Value, ResizeError> {
    let resp = client
        .post(url)
        .json(request)
        .send()
        .await
        .map_err(|source| ResizeError::Request {
            url: url.to_string(),
            source,
        })?;

    let status = resp.status();
    let body = resp.text().await.map_err(|source| ResizeError::Request {
        url: url.to_string(),
        source,
    })?;

    if !status.is_success() {
        return Err(ResizeError::BridgeStatus {
            url: url.to_string(),
            status,
            body,
        });
    }

    let reply: serde_json::Value = serde_json::from_str(&body).map_err(ResizeError::InvalidReply)?;
    if reply.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let error = reply
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown control error")
            .to_string();
        return Err(ResizeError::ControlRejected(error));
    }

    Ok(reply)
}

pub fn cpu_target_request(online: u32) -> serde_json::Value {
    serde_json::json!({ "op": "cpu_target", "online": online })
}

pub fn memory_target_request(total_mib: u32) -> serde_json::Value {
    serde_json::json!({ "op": "memory_target", "total_mib": total_mib })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(cpus: u32, memory: u32, max_cpus: Option<u32>, max_memory: Option<u32>) -> SandboxSpec {
        let mut s = crate::pod::test_support::sandbox().spec;
        s.cpus = cpus;
        s.memory = memory;
        s.max_cpus = max_cpus;
        s.max_memory = max_memory;
        s
    }

    #[test]
    fn headroom_present_when_max_is_set() {
        let s = spec(2, 1024, Some(4), Some(2048));
        assert!(cpus_have_headroom(&s));
        assert!(memory_has_headroom(&s));
    }

    #[test]
    fn no_headroom_when_max_is_unset() {
        let s = spec(1, 512, None, None);
        assert!(!cpus_have_headroom(&s));
        assert!(!memory_has_headroom(&s));
    }

    #[test]
    fn headroom_is_per_dimension() {
        let s = spec(1, 512, Some(4), None);
        assert!(cpus_have_headroom(&s));
        assert!(!memory_has_headroom(&s));
    }

    #[test]
    fn control_url_targets_the_service_dns_and_control_port() {
        assert_eq!(
            control_url("msb-abc123", "team-a", 8080),
            "http://msb-abc123.team-a.svc:8080/control"
        );
    }

    #[test]
    fn cpu_target_request_shape() {
        assert_eq!(
            cpu_target_request(2),
            serde_json::json!({"op": "cpu_target", "online": 2})
        );
    }

    #[test]
    fn memory_target_request_shape() {
        assert_eq!(
            memory_target_request(1024),
            serde_json::json!({"op": "memory_target", "total_mib": 1024})
        );
    }
}
