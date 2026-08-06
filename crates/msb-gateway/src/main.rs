//! msb-gateway: impersonates the upstream microsandbox `CloudBackend` so the
//! **unmodified** msb SDK/CLI (`MSB_API_URL=<gateway>`, `MSB_API_KEY=<k8s token>`)
//! drives the cluster. Two halves, one binary:
//!
//! - **lifecycle** (`lifecycle`) — the REST routes the SDK calls
//!   (`POST/GET/DELETE /v1/sandboxes*`, `/start`, `/stop`): translate the cloud
//!   wire types ↔ our `Sandbox` CRD.
//! - **exec** (`exec`) — `WS /v1/sandboxes/:id/agent`: resolve the sandbox to
//!   its bridge Service and byte-splice the client WS to the bridge WS (never
//!   parses the agent frames).
//!
//! Every request authenticates the Bearer token via TokenReview, derives the
//! caller's namespace from it, and authorizes the per-route verb via
//! SubjectAccessReview.

mod auth;
mod error;
mod exec;
mod lifecycle;
mod limits;
mod logs;
mod resolve;

use std::time::Duration;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use clap::Parser;
use kube::Client;
use tracing::{error, info};

use crate::exec::SpliceConfig;
use crate::limits::ConnLimiter;

#[derive(Parser)]
#[command(name = "msb-gateway")]
struct Cli {
    /// HTTP/WS listen port.
    #[arg(long, env = "MSB_GATEWAY_PORT", default_value_t = 8080)]
    port: u16,

    /// Watch budget (seconds) for create-until-Running. MUST stay < the SDK's
    /// request timeout (default 30s), else create appears to fail.
    #[arg(long, env = "MSB_GATEWAY_CREATE_TIMEOUT_SECS", default_value_t = 25)]
    create_timeout_secs: u64,

    /// Max concurrent exec sessions per identity (0 = unlimited).
    #[arg(
        long,
        env = "MSB_GATEWAY_MAX_SESSIONS_PER_IDENTITY",
        default_value_t = 16
    )]
    max_sessions_per_identity: usize,

    /// Reject any single WS frame larger than this many bytes. Default 64 MiB —
    /// the agent relay's exec-setup frames can be tens of MiB (observed 32 MiB),
    /// so a small cap breaks real exec. Matches tungstenite's own default ceiling.
    #[arg(long, env = "MSB_GATEWAY_MAX_FRAME_BYTES", default_value_t = 64 << 20)]
    max_frame_bytes: usize,

    /// Hard ceiling on a single exec session's wall-clock (seconds). A backstop
    /// against sessions that never close — NOT an idle timeout. 0 disables it.
    #[arg(long, env = "MSB_GATEWAY_MAX_SESSION_SECS", default_value_t = 0)]
    max_session_secs: u64,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) client: Client,
    limiter: ConnLimiter,
    splice_cfg: SpliceConfig,
    pub(crate) create_timeout: Duration,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let client = Client::try_default().await?;
    let state = AppState {
        client,
        limiter: ConnLimiter::new(cli.max_sessions_per_identity),
        splice_cfg: SpliceConfig {
            max_frame_bytes: cli.max_frame_bytes,
            max_session: (cli.max_session_secs > 0)
                .then(|| Duration::from_secs(cli.max_session_secs)),
        },
        create_timeout: Duration::from_secs(cli.create_timeout_secs),
    };

    let app = Router::new()
        .route("/v1/sandboxes/{name}/agent", get(exec_handler))
        .merge(lifecycle::routes())
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cli.port)).await?;
    info!(port = cli.port, "gateway listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// `GET /v1/sandboxes/:name/agent` — auth, resolve, upgrade, splice.
async fn exec_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Auth is checked BEFORE upgrading, so a rejection is a clean HTTP error the
    // SDK maps to a typed code (not a mid-stream WS close). The namespace is
    // derived from the token.
    let token = match auth::bearer_token(auth::auth_header(&headers)) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    let identity = match auth::authorize(&state.client, &token, &name).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let ns = identity.namespace.clone();

    // Enforce the per-identity concurrent-session cap before doing more work. The
    // guard is held for the whole session and released on drop.
    let guard = match state.limiter.acquire(&identity.username) {
        Some(g) => g,
        None => {
            return (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                axum::Json(serde_json::json!({
                    "code": "too_many_sessions",
                    "message": "per-identity concurrent exec-session limit reached",
                })),
            )
                .into_response();
        }
    };

    // Resolve to the bridge URL before upgrading, for the same reason.
    let target = match resolve::resolve(&state.client, &ns, &name).await {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

    // Cap the client leg's frame/message size (the bridge leg is capped on dial).
    let cfg = state.splice_cfg;
    let ws = ws
        .max_frame_size(cfg.max_frame_bytes)
        .max_message_size(cfg.max_frame_bytes);
    let bridge_url = target.url;
    ws.on_upgrade(move |socket| async move {
        // Hold the slot for the session lifetime; dropped when the splice ends.
        let _guard = guard;
        if let Err(e) = exec::splice(socket, &bridge_url, cfg).await {
            error!(sandbox = %name, error = %e, "exec splice failed");
        }
    })
}
