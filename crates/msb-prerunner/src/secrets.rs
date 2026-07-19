//! Resolving `spec.secrets[]` into the plaintext `SecretEntry` values msb wants.
//!
//! The guest's env var holds a *placeholder*, never the real value; msb's proxy
//! swaps placeholder → value in outbound traffic to `allowed_hosts`. So the
//! plaintext lives only in the config on the tmpfs and in msb's memory.

use std::collections::BTreeMap;

use msb_crd::sandbox::SecretEntry as SpecSecret;

use crate::launch::{HostPattern, SecretEntry};

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret for env {env}: Secret {secret}/{key} not found or key missing")]
    Missing {
        env: String,
        secret: String,
        key: String,
    },

    // Two entries sharing an env var would derive the same placeholder but carry
    // different values — msb's proxy would then substitute non-deterministically.
    #[error("secret env var {env} is used by more than one entry")]
    DuplicateEnv { env: String },

    #[error("secret for env {env}: invalid allowed_host {host:?}")]
    InvalidHost { env: String, host: String },
}

/// Deduplicated `(secret_name, key)` pairs to fetch — many entries may share one.
pub fn required_keys(specs: &[SpecSecret]) -> Vec<(String, String)> {
    let mut seen = std::collections::BTreeSet::new();
    specs
        .iter()
        .map(|s| {
            (
                s.value_from.secret_key_ref.name.clone(),
                s.value_from.secret_key_ref.key.clone(),
            )
        })
        .filter(|pair| seen.insert(pair.clone()))
        .collect()
}

/// The guest env var holds this, not the value. Deterministic and valid per
/// msb's rules (non-empty, ASCII, no NUL/CR/LF).
fn placeholder_for(env_var: &str) -> String {
    format!("msb_secret_{env_var}")
}

/// Translates CRD secrets into resolved `SecretEntry`s. `resolved` maps
/// `(secret_name, key)` to the fetched plaintext; a missing entry is a hard
/// error so the pod fails to start rather than booting with an empty secret.
pub fn resolve(
    specs: &[SpecSecret],
    resolved: &BTreeMap<(String, String), String>,
) -> Result<Vec<SecretEntry>, SecretError> {
    let mut env_seen = std::collections::BTreeSet::new();
    specs
        .iter()
        .map(|s| {
            if !env_seen.insert(&s.env) {
                return Err(SecretError::DuplicateEnv { env: s.env.clone() });
            }
            let name = &s.value_from.secret_key_ref.name;
            let key = &s.value_from.secret_key_ref.key;
            let value = resolved
                .get(&(name.clone(), key.clone()))
                .ok_or_else(|| SecretError::Missing {
                    env: s.env.clone(),
                    secret: name.clone(),
                    key: key.clone(),
                })?
                .clone();
            let allowed_hosts = s
                .allowed_hosts
                .iter()
                .map(|h| host_pattern(&s.env, h))
                .collect::<Result<_, _>>()?;
            Ok(SecretEntry {
                env_var: s.env.clone(),
                value,
                placeholder: placeholder_for(&s.env),
                allowed_hosts,
                require_tls_identity: true,
            })
        })
        .collect()
}

/// `*.suffix` → wildcard, else an exact host. Rejects garbage that would
/// silently mis-substitute: a bare `*`, an empty host, or `*.` with no suffix.
fn host_pattern(env: &str, host: &str) -> Result<HostPattern, SecretError> {
    let invalid = || SecretError::InvalidHost {
        env: env.to_string(),
        host: host.to_string(),
    };
    match host.strip_prefix("*.") {
        Some("") => Err(invalid()),
        Some(suffix) => Ok(HostPattern::Wildcard(suffix.to_string())),
        None if host.is_empty() || host.contains('*') => Err(invalid()),
        None => Ok(HostPattern::Exact(host.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use msb_crd::sandbox::{SecretKeyRef, SecretValueFrom};

    fn spec_secret(env: &str, secret: &str, key: &str, hosts: &[&str]) -> SpecSecret {
        SpecSecret {
            env: env.to_string(),
            value_from: SecretValueFrom {
                secret_key_ref: SecretKeyRef {
                    name: secret.to_string(),
                    key: key.to_string(),
                },
            },
            allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
        }
    }

    #[test]
    fn resolves_value_and_derives_placeholder() {
        let specs = vec![spec_secret("API_KEY", "creds", "api", &["api.openai.com"])];
        let resolved = BTreeMap::from([(("creds".into(), "api".into()), "sk-live-xyz".into())]);

        let out = resolve(&specs, &resolved).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].env_var, "API_KEY");
        assert_eq!(out[0].value, "sk-live-xyz");
        assert_eq!(out[0].placeholder, "msb_secret_API_KEY");
        assert!(out[0].require_tls_identity);
    }

    #[test]
    fn missing_secret_is_an_error_not_an_empty_value() {
        let specs = vec![spec_secret("API_KEY", "creds", "api", &[])];
        let err = resolve(&specs, &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, SecretError::Missing { .. }));
    }

    #[test]
    fn wildcard_and_exact_hosts() {
        let specs = vec![spec_secret("K", "s", "k", &["*.openai.com", "exact.com"])];
        let resolved = BTreeMap::from([(("s".into(), "k".into()), "v".into())]);
        let out = resolve(&specs, &resolved).unwrap();
        assert!(matches!(&out[0].allowed_hosts[0], HostPattern::Wildcard(h) if h == "openai.com"));
        assert!(matches!(&out[0].allowed_hosts[1], HostPattern::Exact(h) if h == "exact.com"));
    }

    #[test]
    fn duplicate_env_var_is_rejected() {
        let specs = vec![
            spec_secret("API_KEY", "a", "k", &[]),
            spec_secret("API_KEY", "b", "k", &[]),
        ];
        let resolved = BTreeMap::from([
            (("a".into(), "k".into()), "v1".into()),
            (("b".into(), "k".into()), "v2".into()),
        ]);
        assert!(matches!(
            resolve(&specs, &resolved).unwrap_err(),
            SecretError::DuplicateEnv { .. }
        ));
    }

    #[test]
    fn garbage_hosts_are_rejected() {
        let resolved = BTreeMap::from([(("s".into(), "k".into()), "v".into())]);
        for bad in ["*", "", "*.", "a*b"] {
            let specs = vec![spec_secret("K", "s", "k", &[bad])];
            assert!(
                matches!(
                    resolve(&specs, &resolved).unwrap_err(),
                    SecretError::InvalidHost { .. }
                ),
                "host {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn required_keys_dedupes() {
        let specs = vec![
            spec_secret("A", "creds", "shared", &[]),
            spec_secret("B", "creds", "shared", &[]),
            spec_secret("C", "creds", "other", &[]),
        ];
        let keys = required_keys(&specs);
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn placeholder_is_valid_per_msb_rules() {
        let p = placeholder_for("SOME_VAR");
        assert!(!p.is_empty());
        assert!(p.len() <= 1024);
        assert!(!p.contains(['\0', '\r', '\n']));
    }
}
