//! The exec path: splice the SDK's WS to the sandbox's bridge WS as a transparent
//! byte pipe. The 0.6.8 agent client owns the relay's `[id_min][id_max]` +
//! `core.ready` prologue and needs the stream verbatim, so the gateway forwards
//! bytes untouched in both directions — no handshake swallow, no id rewrite.

use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use axum::extract::ws::{Message as AxumMsg, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message as TungMsg;
use tracing::{debug, warn};

use crate::error::GatewayError;

#[derive(Debug, Clone, Copy)]
pub struct SpliceConfig {
    pub max_frame_bytes: usize,
    /// A backstop against sessions that never close, NOT an idle timeout — an idle
    /// shell stays connected until this elapses. `None` = no ceiling.
    pub max_session: Option<Duration>,
}

impl SpliceConfig {
    fn ws_config(&self) -> WebSocketConfig {
        WebSocketConfig {
            max_message_size: Some(self.max_frame_bytes),
            max_frame_size: Some(self.max_frame_bytes),
            ..Default::default()
        }
    }
}

/// One fresh bridge dial per exec session — no multiplexing — so the relay's
/// per-connection id isolation holds. Backpressure is inherent: each forward is
/// `send().await`, which propagates TCP backpressure and bounds buffering.
pub async fn splice(
    client: WebSocket,
    bridge_url: &str,
    cfg: SpliceConfig,
) -> Result<(), GatewayError> {
    let bridge = dial_bridge_with_retry(bridge_url, cfg).await?;
    debug!(bridge_url, "bridge dialed; splicing");

    let (mut client_tx, mut client_rx) = client.split();
    let (mut bridge_tx, mut bridge_rx) = bridge.split();

    let c2b = async {
        while let Some(msg) = client_rx.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, "client ws recv error");
                    break;
                }
            };
            let bytes = match msg {
                AxumMsg::Binary(data) => data,
                AxumMsg::Text(t) => t.into_bytes(),
                AxumMsg::Close(_) => break,
                // pings/pongs are handled by the transport; nothing to forward.
                _ => continue,
            };
            if bridge_tx.send(TungMsg::Binary(bytes)).await.is_err() {
                break;
            }
        }
        let _ = bridge_tx.close().await;
    };

    let b2c = async {
        while let Some(msg) = bridge_rx.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, "bridge ws recv error");
                    break;
                }
            };
            let bytes = match msg {
                TungMsg::Binary(data) => data,
                TungMsg::Text(t) => t.into_bytes(),
                TungMsg::Close(_) => break,
                _ => continue,
            };
            if client_tx.send(AxumMsg::Binary(bytes)).await.is_err() {
                break;
            }
        }
        let _ = client_tx.close().await;
    };

    let both = async {
        tokio::select! {
            _ = c2b => {}
            _ = b2c => {}
        }
    };
    match cfg.max_session {
        Some(limit) => {
            if tokio::time::timeout(limit, both).await.is_err() {
                warn!("exec session exceeded max wall-clock; tearing down");
            }
        }
        None => both.await,
    }
    Ok(())
}

// The sandbox reaches Running a beat before the bridge sidecar binds :7000, so
// retry the dial for a bounded window rather than fail the exec.
async fn dial_bridge_with_retry(
    bridge_url: &str,
    cfg: SpliceConfig,
) -> Result<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    GatewayError,
> {
    // Bounded by wall-clock, not attempts: the budget is client patience, which
    // must stay under the SDK's exec timeout.
    const DEADLINE: Duration = Duration::from_secs(8);

    let policy = ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(20))
        .with_max_delay(Duration::from_millis(500))
        .with_jitter()
        .without_max_times();

    // Per-attempt timeout: a dial to a Service with no ready endpoints (bridge
    // sidecar not yet bound) blackholes the SYN rather than refusing, so without
    // this the first attempt hangs the whole deadline and the retry never fires.
    const ATTEMPT: Duration = Duration::from_millis(500);
    let dial = || async {
        let connect =
            tokio_tungstenite::connect_async_with_config(bridge_url, Some(cfg.ws_config()), false);
        match tokio::time::timeout(ATTEMPT, connect).await {
            Ok(res) => res
                .map(|(bridge, _resp)| bridge)
                .map_err(|e| GatewayError::Bridge(format!("connect: {e}"))),
            Err(_) => Err(GatewayError::Bridge("connect attempt timed out".into())),
        }
    };

    match tokio::time::timeout(DEADLINE, dial.retry(policy).sleep(tokio::time::sleep)).await {
        Ok(Ok(bridge)) => Ok(bridge),
        Ok(Err(e)) => Err(GatewayError::Bridge(format!("dial {bridge_url} failed: {e}"))),
        Err(_) => Err(GatewayError::Bridge(format!(
            "dial {bridge_url} not ready within {DEADLINE:?}"
        ))),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::WebSocketUpgrade;
    use axum::extract::State;
    use axum::response::Response;
    use axum::routing::get;
    use axum::Router;
    use std::sync::Arc;
    use tokio_tungstenite::tungstenite::Message as TungMsg;

    fn test_cfg() -> SpliceConfig {
        SpliceConfig {
            max_frame_bytes: 4 << 20,
            max_session: None,
        }
    }

    async fn gateway_route(
        State(bridge_url): State<Arc<String>>,
        ws: WebSocketUpgrade,
    ) -> Response {
        let bridge_url = bridge_url.clone();
        ws.on_upgrade(move |socket| async move {
            let _ = splice(socket, &bridge_url, test_cfg()).await;
        })
    }

    async fn spawn_gateway(bridge_url: String) -> u16 {
        let app = Router::new()
            .route("/ws", get(gateway_route))
            .with_state(Arc::new(bridge_url));
        let gw = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = gw.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(gw, app).await.unwrap() });
        port
    }

    // A bridge that sends a prologue on connect, then echoes what it receives.
    async fn echo_bridge(port_tx: tokio::sync::oneshot::Sender<u16>, prologue: Vec<u8>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        port_tx.send(listener.local_addr().unwrap().port()).unwrap();
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(TungMsg::Binary(prologue)).await.unwrap();
        while let Some(Ok(msg)) = ws.next().await {
            match msg {
                TungMsg::Binary(d) => ws.send(TungMsg::Binary(d)).await.unwrap(),
                TungMsg::Close(_) => break,
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn splice_forwards_bytes_verbatim_both_directions() {
        // The bridge's prologue (id-range + ready) must reach the client untouched,
        // and the client's bytes must reach the bridge untouched.
        let prologue = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04, 0x99];
        let (ptx, prx) = tokio::sync::oneshot::channel();
        tokio::spawn(echo_bridge(ptx, prologue.clone()));
        let bridge_url = format!("ws://127.0.0.1:{}/", prx.await.unwrap());
        let gw_port = spawn_gateway(bridge_url).await;

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{gw_port}/ws"))
                .await
                .unwrap();

        let got = client.next().await.unwrap().unwrap().into_data();
        assert_eq!(got, prologue, "bridge prologue must pass through verbatim");

        let payload = b"exec-request".to_vec();
        client.send(TungMsg::Binary(payload.clone())).await.unwrap();
        let echoed = client.next().await.unwrap().unwrap().into_data();
        assert_eq!(echoed, payload, "client bytes must pass through verbatim");
    }

    async fn silent_bridge(port_tx: tokio::sync::oneshot::Sender<u16>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        port_tx.send(listener.local_addr().unwrap().port()).unwrap();
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(TungMsg::Binary(vec![0u8])).await.unwrap();
        while let Some(Ok(_)) = ws.next().await {}
    }

    async fn capped_route(
        State(bridge_url): State<Arc<String>>,
        ws: WebSocketUpgrade,
    ) -> Response {
        let bridge_url = bridge_url.clone();
        ws.on_upgrade(move |socket| async move {
            let cfg = SpliceConfig {
                max_frame_bytes: 4 << 20,
                max_session: Some(std::time::Duration::from_millis(300)),
            };
            let _ = splice(socket, &bridge_url, cfg).await;
        })
    }

    #[tokio::test]
    async fn max_session_ceiling_tears_down_a_stuck_session() {
        let (ptx, prx) = tokio::sync::oneshot::channel();
        tokio::spawn(silent_bridge(ptx));
        let bridge_url = format!("ws://127.0.0.1:{}/", prx.await.unwrap());

        let app = Router::new()
            .route("/ws", get(capped_route))
            .with_state(Arc::new(bridge_url));
        let gw = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let gw_port = gw.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(gw, app).await.unwrap() });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{gw_port}/ws"))
                .await
                .unwrap();
        let closed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                match client.next().await {
                    None | Some(Ok(TungMsg::Close(_))) | Some(Err(_)) => break true,
                    Some(Ok(_)) => continue,
                }
            }
        })
        .await;
        assert_eq!(closed, Ok(true), "session ceiling did not tear down the session");
    }
}
