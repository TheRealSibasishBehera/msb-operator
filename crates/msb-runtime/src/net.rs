//! Maps the CRD's network spec and the resolved secrets onto the SDK network
//! builder. Without this the sandbox boots with no policy and no
//! secret substitution — the SDK applies nothing we don't set here.

use std::net::{IpAddr, Ipv4Addr};

use microsandbox::NetworkPolicy;
use microsandbox::sandbox::SandboxBuilder;
use msb_crd::sandbox::{PolicyPreset, PortProtocol};
use msb_crd::{ResolvedSecret, SandboxSpec};

/// Applies `spec.network` and `secrets` to the builder via its `.network()`
/// closure. Called unconditionally; when networking is disabled it still sets
/// `enabled(false)` so the default (enabled) does not leak through.
pub fn apply(
    builder: SandboxBuilder,
    spec: &SandboxSpec,
    secrets: &[ResolvedSecret],
) -> SandboxBuilder {
    let net = spec.network.clone();
    let secrets = secrets.to_vec();

    builder.network(move |mut n| {
        n = n.enabled(net.enabled).policy(policy(&net.policy.preset));

        for p in &net.published_ports {
            let bind: IpAddr = p.host_bind.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
            let (host, guest) = (p.host_port(), p.guest_port);
            n = match p.protocol {
                PortProtocol::Tcp => n.port_bind(bind, host, guest),
                PortProtocol::Udp => n.port_udp_bind(bind, host, guest),
            };
        }

        if net.tls.intercept {
            let ports: Vec<u16> = net.tls.intercepted_ports.iter().map(|p| p.port).collect();
            n = n.tls(move |t| t.intercepted_ports(ports));
        }

        n = n
            .max_connections(net.max_connections as usize)
            .trust_host_cas(net.trust_host_cas);

        for s in &secrets {
            let s = s.clone();
            n = n.secret(move |mut b| {
                b = b.env(&s.env).value(&s.value).placeholder(&s.placeholder);
                for host in &s.allowed_hosts {
                    // `*.suffix` is a wildcard pattern; anything else is exact.
                    b = match host.strip_prefix("*.") {
                        Some(_) => b.allow_host_pattern(host),
                        None => b.allow_host(host),
                    };
                }
                b
            });
        }

        n
    })
}

fn policy(preset: &PolicyPreset) -> NetworkPolicy {
    use microsandbox::NetworkProfile::{Private, Public};
    match preset {
        PolicyPreset::PublicOnly => NetworkPolicy::from_profiles([Public]),
        PolicyPreset::AllowAll => NetworkPolicy::allow_all(),
        PolicyPreset::DenyAll => NetworkPolicy::none(),
        PolicyPreset::NonLocal => NetworkPolicy::from_profiles([Public, Private]),
    }
}
