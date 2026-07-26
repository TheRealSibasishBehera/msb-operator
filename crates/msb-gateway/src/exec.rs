//! The exec path: splice the SDK's WS to the sandbox's bridge WS, rewriting only
//! the correlation-id field so the SDK's fixed id fits the relay's assigned range.

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

    // The SDK doesn't expect the relay's handshake prologue; swallow it (bounded so
    // a bridge that never handshakes can't hang the session).
    let handshake = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        swallow_relay_handshake(&mut bridge_rx, cfg.max_frame_bytes),
    )
    .await;
    let (id_start, leftover) = match handshake {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            warn!(error = %e, "relay handshake consume failed");
            let _ = bridge_tx.close().await;
            let _ = client_tx.close().await;
            return Err(GatewayError::Bridge(format!("relay handshake: {e}")));
        }
        Err(_) => {
            warn!("relay handshake timed out");
            let _ = bridge_tx.close().await;
            let _ = client_tx.close().await;
            return Err(GatewayError::Bridge("relay handshake timed out".into()));
        }
    };
    debug!(id_start, "relay handshake consumed");

    // The SDK hardcodes id=1 but the relay rejects ids outside its assigned range;
    // translate 1<->id_start.
    let mut c2b_rw = FrameIdRewriter::new(1, id_start, cfg.max_frame_bytes);
    let mut b2c_rw = FrameIdRewriter::new(id_start, 1, cfg.max_frame_bytes);

    // Leftover bytes past the handshake start the first response frame.
    if !leftover.is_empty() {
        match b2c_rw.push(&leftover) {
            Ok(out) if !out.is_empty() => {
                if client_tx.send(AxumMsg::Binary(out)).await.is_err() {
                    let _ = bridge_tx.close().await;
                    return Ok(());
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "leftover reframe failed");
                let _ = bridge_tx.close().await;
                let _ = client_tx.close().await;
                return Err(GatewayError::Bridge(e));
            }
        }
    }

    // client -> bridge: reframe (id 1 -> id_start), forward; a client close ends it.
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
            match c2b_rw.push(&bytes) {
                Ok(out) if out.is_empty() => {}
                Ok(out) => {
                    if bridge_tx.send(TungMsg::Binary(out)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "client->bridge reframe error");
                    break;
                }
            }
        }
        let _ = bridge_tx.close().await;
    };

    // bridge -> client: reframe (id id_start -> 1), forward; a bridge close ends it.
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
            match b2c_rw.push(&bytes) {
                Ok(out) if out.is_empty() => {}
                Ok(out) => {
                    if client_tx.send(AxumMsg::Binary(out)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "bridge->client reframe error");
                    break;
                }
            }
        }
        let _ = client_tx.close().await;
    };

    // Either direction ending — or the session ceiling — tears down the session.
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

// Agent frame: `[len:u32 BE][id:u32 BE][flags:u8][CBOR body]`; `len` counts
// everything after the 4-byte length prefix.
const LEN_PREFIX: usize = 4;
const FRAME_HEADER: usize = 5;
// The relay exempts id-0 shutdown frames from range enforcement, so never rewrite them.
const FLAG_SHUTDOWN: u8 = 0b0000_0100;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

// Rewrites the id field of each frame in one direction. WS message boundaries
// aren't frame boundaries, so bytes are accumulated and frames cut by their `len`
// prefix — the CBOR body is never decoded, only the fixed-offset id.
struct FrameIdRewriter {
    buf: Vec<u8>,
    from_id: u32,
    to_id: u32,
    max_frame: usize,
}

impl FrameIdRewriter {
    fn new(from_id: u32, to_id: u32, max_frame: usize) -> Self {
        Self { buf: Vec::new(), from_id, to_id, max_frame }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < LEN_PREFIX {
                break;
            }
            let len =
                u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if len < FRAME_HEADER {
                return Err(format!("frame len {len} < header {FRAME_HEADER}"));
            }
            if len > self.max_frame {
                return Err(format!("frame len {len} exceeds max {}", self.max_frame));
            }
            let total = LEN_PREFIX + len;
            if self.buf.len() < total {
                break; // frame incomplete; wait for more bytes
            }
            let mut frame: Vec<u8> = self.buf.drain(..total).collect();
            // id = bytes 4..8, flags = byte 8.
            let id = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
            let flags = frame[8];
            let is_shutdown = flags & FLAG_SHUTDOWN != 0 && id == 0;
            if !is_shutdown && id == self.from_id {
                frame[4..8].copy_from_slice(&self.to_id.to_be_bytes());
            }
            out.extend_from_slice(&frame);
        }
        Ok(out)
    }
}

// Consume the relay handshake — `[id_start:u32][id_end:u32]` + a length-prefixed
// ready frame — returning `id_start` and any bytes read past it.
async fn swallow_relay_handshake(
    bridge_rx: &mut (impl StreamExt<
        Item = Result<TungMsg, tokio_tungstenite::tungstenite::Error>,
    > + Unpin),
    max_frame: usize,
) -> Result<(u32, Vec<u8>), String> {
    let mut buf: Vec<u8> = Vec::new();

    // Pull WS binary frames until we have enough bytes to consume the handshake.
    async fn ensure(
        buf: &mut Vec<u8>,
        need: usize,
        rx: &mut (impl StreamExt<
            Item = Result<TungMsg, tokio_tungstenite::tungstenite::Error>,
        > + Unpin),
    ) -> Result<(), String> {
        while buf.len() < need {
            match rx.next().await {
                Some(Ok(TungMsg::Binary(b))) => buf.extend_from_slice(&b),
                Some(Ok(TungMsg::Text(t))) => buf.extend_from_slice(t.as_bytes()),
                Some(Ok(_)) => {} // ping/pong: ignore
                Some(Err(e)) => return Err(format!("bridge recv: {e}")),
                None => return Err("bridge closed before handshake completed".into()),
            }
        }
        Ok(())
    }

    // [id_start:u32][id_end:u32] then [len:u32][id][flags][body].
    ensure(&mut buf, 8, bridge_rx).await?;
    let id_start = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    ensure(&mut buf, 12, bridge_rx).await?;
    let frame_len = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    // Bound up front so a hostile handshake can't force unbounded accumulation.
    if frame_len > max_frame {
        return Err(format!("ready frame len {frame_len} exceeds max {max_frame}"));
    }
    let total = 8 + 4 + frame_len;
    ensure(&mut buf, total, bridge_rx).await?;

    Ok((id_start, buf.split_off(total)))
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

    let dial = || async {
        tokio_tungstenite::connect_async_with_config(bridge_url, Some(cfg.ws_config()), false)
            .await
            .map(|(bridge, _resp)| bridge)
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

    /// Build one agent frame: [len:u32][id:u32][flags:u8][body].
    fn make_frame(id: u32, flags: u8, body: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        let len = (FRAME_HEADER + body.len()) as u32;
        f.extend_from_slice(&len.to_be_bytes());
        f.extend_from_slice(&id.to_be_bytes());
        f.push(flags);
        f.extend_from_slice(body);
        f
    }

    fn frame_id(f: &[u8]) -> u32 {
        u32::from_be_bytes([f[4], f[5], f[6], f[7]])
    }

    #[test]
    fn rewrites_matching_id_and_leaves_body_untouched() {
        let mut rw = FrameIdRewriter::new(1, 42, 64 << 20);
        let frame = make_frame(1, 0, b"hello-body");
        let out = rw.push(&frame).unwrap();
        assert_eq!(frame_id(&out), 42, "id rewritten 1->42");
        assert_eq!(&out[9..], b"hello-body", "body untouched");
        assert_eq!(out.len(), frame.len(), "len untouched");
        assert_eq!(out[8], 0, "flags untouched");
    }

    #[test]
    fn frame_split_across_two_pushes_reassembles() {
        let mut rw = FrameIdRewriter::new(1, 7, 64 << 20);
        let frame = make_frame(1, 0, b"abcdefghij");
        let (a, b) = frame.split_at(6); // split mid-frame
        assert!(rw.push(a).unwrap().is_empty(), "partial frame not emitted");
        let out = rw.push(b).unwrap();
        assert_eq!(frame_id(&out), 7);
        assert_eq!(&out[9..], b"abcdefghij");
    }

    #[test]
    fn two_frames_in_one_push_both_rewritten() {
        let mut rw = FrameIdRewriter::new(1, 9, 64 << 20);
        let mut bytes = make_frame(1, 0, b"one");
        bytes.extend_from_slice(&make_frame(1, 0, b"two"));
        let out = rw.push(&bytes).unwrap();
        // Two frames back-to-back, both id 9.
        assert_eq!(frame_id(&out), 9);
        let second = &out[9 + 3..]; // after first frame (header+3 body)
        assert_eq!(frame_id(second), 9);
    }

    #[test]
    fn shutdown_frame_id0_not_rewritten() {
        let mut rw = FrameIdRewriter::new(1, 5, 64 << 20);
        let frame = make_frame(0, FLAG_SHUTDOWN, b"x");
        let out = rw.push(&frame).unwrap();
        assert_eq!(frame_id(&out), 0, "shutdown id 0 stays 0");
    }

    #[test]
    fn non_matching_id_passes_through() {
        let mut rw = FrameIdRewriter::new(1, 5, 64 << 20);
        let frame = make_frame(3, 0, b"x"); // id 3, not from_id 1
        let out = rw.push(&frame).unwrap();
        assert_eq!(frame_id(&out), 3, "unrelated id untouched");
    }

    #[test]
    fn oversized_len_is_an_error() {
        let mut rw = FrameIdRewriter::new(1, 5, 1024);
        // Hand-craft a header claiming a 2 MiB frame.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(2u32 * 1024 * 1024).to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.push(0);
        assert!(rw.push(&bytes).is_err());
    }

    // A minimal valid relay handshake: [id_start:u32 BE][id_end:u32 BE] then a
    // length-prefixed frame [len:u32 BE][id:u32 BE][flags:u8][body]. The gateway
    // must swallow all of this. `id_start` is the correlation range start the
    // relay assigns; the gateway translates the SDK's id=1 to/from it.
    const TEST_ID_START: u32 = 1000;
    fn relay_handshake() -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(&TEST_ID_START.to_be_bytes()); // id_start
        h.extend_from_slice(&2000u32.to_be_bytes()); // id_end_exclusive
        let body = [0xABu8]; // 1-byte fake ready body
        let frame_len = (5 + body.len()) as u32; // FRAME_HEADER_SIZE(5) + body
        h.extend_from_slice(&frame_len.to_be_bytes()); // len prefix
        h.extend_from_slice(&7u32.to_be_bytes()); // id
        h.push(0u8); // flags
        h.extend_from_slice(&body); // body
        h
    }

    // A fake bridge that (1) sends the handshake (swallowed by the gateway),
    // (2) asserts frames it RECEIVES from the client carry id_start (proving
    // client->bridge translation 1->id_start), then (3) echoes each received
    // frame back — the gateway must translate id_start->1 on the way to the client.
    async fn fake_bridge(port_tx: tokio::sync::oneshot::Sender<u16>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        port_tx.send(port).unwrap();
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(TungMsg::Binary(relay_handshake())).await.unwrap();
        while let Some(Ok(msg)) = ws.next().await {
            match msg {
                TungMsg::Binary(d) => {
                    // The client sent id=1; the gateway must have rewritten it to id_start.
                    assert_eq!(
                        u32::from_be_bytes([d[4], d[5], d[6], d[7]]),
                        TEST_ID_START,
                        "client->bridge frame id must be translated to id_start"
                    );
                    ws.send(TungMsg::Binary(d)).await.unwrap(); // echo (id_start)
                }
                TungMsg::Close(_) => break,
                _ => {}
            }
        }
    }

    // Mount the real `splice` behind an axum WS route, exactly as main.rs does.
    async fn gateway_route(
        State(bridge_url): State<Arc<String>>,
        ws: WebSocketUpgrade,
    ) -> Response {
        let bridge_url = bridge_url.clone();
        ws.on_upgrade(move |socket| async move {
            let _ = splice(socket, &bridge_url, test_cfg()).await;
        })
    }

    fn test_cfg() -> SpliceConfig {
        SpliceConfig {
            max_frame_bytes: 4 << 20,
            max_session: None, // no ceiling in the happy-path echo test
        }
    }

    #[tokio::test]
    async fn translates_ids_both_directions_end_to_end() {
        // 1. Fake bridge.
        let (ptx, prx) = tokio::sync::oneshot::channel();
        tokio::spawn(fake_bridge(ptx));
        let bridge_port = prx.await.unwrap();
        let bridge_url = format!("ws://127.0.0.1:{bridge_port}/");

        // 2. Gateway serving the real splice.
        let app = Router::new()
            .route("/ws", get(gateway_route))
            .with_state(Arc::new(bridge_url));
        let gw = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let gw_port = gw.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(gw, app).await.unwrap() });

        // 3. A real WS client (the SDK's role) sends a frame with id=1.
        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{gw_port}/ws"))
                .await
                .unwrap();
        client
            .send(TungMsg::Binary(make_frame(1, 0, b"exec-request")))
            .await
            .unwrap();

        // The echoed frame comes back with id translated back to 1.
        let echoed = client.next().await.unwrap().unwrap();
        let data = echoed.into_data();
        assert_eq!(
            u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
            1,
            "bridge->client frame id must be translated back to 1"
        );
        assert_eq!(&data[9..], b"exec-request", "body preserved");
    }


    // A bridge that completes the handshake, then stays open but never sends
    // again — so only the session ceiling can end the splice.
    async fn silent_bridge(port_tx: tokio::sync::oneshot::Sender<u16>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        port_tx.send(port).unwrap();
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(TungMsg::Binary(relay_handshake())).await.unwrap();
        while let Some(Ok(_)) = ws.next().await {}
    }

    async fn capped_route(
        State(bridge_url): State<Arc<String>>,
        ws: WebSocketUpgrade,
    ) -> Response {
        let bridge_url = bridge_url.clone();
        ws.on_upgrade(move |socket| async move {
            // A short session ceiling so the test is fast.
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
        let bridge_port = prx.await.unwrap();
        let bridge_url = format!("ws://127.0.0.1:{bridge_port}/");

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

        // Nothing flows; the 300ms ceiling must close the client within a bound.
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
