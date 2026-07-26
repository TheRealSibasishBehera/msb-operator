//! The sandbox **wire contract** for this operator.
//!
//! # We import the contract types — we do not hand-roll them
//!
//! The config / status / wire types are **imported** from the published
//! [`microsandbox_types`] crate (0.6.6) — its stated purpose is *"shared task and
//! wire contract types for microsandbox"*, and it is deliberately lightweight
//! (`serde` + `chrono` only — verified: importing it drags in NO libkrun / agentd
//! / nix / tokio). Re-inventing the create request / sandbox / status types here
//! would just duplicate them and drift from upstream. So this crate:
//!
//!   - **re-exports** the upstream wire types as our vocabulary (below).
//!
//! It holds nothing else. The wire↔CRD **mapping** (request→spec, phase→status,
//! "find every field a home") is NOT here — depending on `msb-crd` would drag
//! `k8s-openapi` into this lightweight crate. If a client-side access helper is
//! built (see `docs/client-access-design.md`), the mapping lives there, where the
//! CRD is already a dependency. It is not built in-tree yet.
//!
//! What we do NOT import is the upstream `SandboxBackend` **trait** — its return
//! types (`Sandbox`/`SandboxHandle`) embed libkrun process handles + agentd UDS
//! connections, i.e. the local-VM runtime, unsuitable for a CRD-backed control
//! path. And we deliberately do not define a behavioural trait of our own yet:
//! there is a single realization (Kubernetes API + per-sandbox bridge), reached
//! over different *network paths* (in-cluster / port-forward / ingress) that do
//! not change the code — so a behaviour-swap abstraction would be one impl of a
//! speculative trait. Add it in V2 if a second realization (e.g. a test fake)
//! earns it. Until then this crate is data + mapping, not behaviour.

// Re-export the upstream wire types as this operator's contract vocabulary.
// Anchored on the flat *cloud/wire* shapes (what a client sends and receives),
// not the heavier full-domain `SandboxSpec`.
pub use microsandbox_types::{
    CloudCreateSandboxRequest, CloudMessageResponse, CloudPaginated, CloudSandbox,
    CloudSandboxStatus,
};
