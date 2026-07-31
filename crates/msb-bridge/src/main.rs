//! The msb-bridge sidecar: exposes the sandbox's agent relay socket over
//! WebSocket so clients elsewhere in the cluster can reach it.
//!
//! Each WebSocket connection dials the relay fresh (the relay handshakes it and
//! assigns a session id-range), then bytes flow both ways verbatim — the relay
//! handles multiplexing, so the bridge is a transparent byte proxy.

mod socket;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "msb-bridge")]
struct Cli {
    /// msb's flat sandbox name — used to locate the relay socket.
    #[arg(long, env = "MSB_SANDBOX_NAME")]
    sandbox_name: String,

    #[arg(long, default_value = "/msb", env = "MSB_HOME")]
    msb_home: PathBuf,

    /// WebSocket listen port.
    #[arg(long, default_value_t = 7000, env = "MSB_BRIDGE_PORT")]
    port: u16,

    /// Health-check listen port.
    #[arg(long, default_value_t = 8080, env = "MSB_BRIDGE_HEALTH_PORT")]
    health_port: u16,

    /// How long a connection retries dialing the socket before giving up.
    #[arg(long, default_value_t = 60, env = "MSB_BRIDGE_DIAL_TIMEOUT_SECS")]
    dial_timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let sock_path = socket::socket_path(&cli.msb_home, &cli.sandbox_name);
    // Healthy once we have served at least one connection whose socket dial
    // succeeded (the sandbox is up and reachable).
    let ready = Arc::new(AtomicBool::new(false));

    tokio::spawn(serve_health(cli.health_port, ready.clone()));

    let listener = TcpListener::bind(("0.0.0.0", cli.port))
        .await
        .with_context(|| format!("binding ws listener on :{}", cli.port))?;
    info!(port = cli.port, sock = %sock_path.display(), "bridge listening");

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "accept failed");
                continue;
            }
        };
        let sock_path = sock_path.clone();
        let ready = ready.clone();
        let dial_timeout = Duration::from_secs(cli.dial_timeout_secs);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(tcp, &sock_path, dial_timeout, &ready).await {
                warn!(%peer, error = %e, "connection ended with error");
            }
        });
    }
}

/// Upgrades one TCP connection to WebSocket, dials the relay, and proxies bytes.
async fn handle_conn(
    tcp: tokio::net::TcpStream,
    sock_path: &std::path::Path,
    dial_timeout: Duration,
    ready: &AtomicBool,
) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(tcp)
        .await
        .context("websocket handshake")?;
    let uds = socket::dial(sock_path, dial_timeout).await?;
    ready.store(true, Ordering::Relaxed);
    proxy(ws, uds).await
}

/// Bidirectional copy: WS binary frames -> socket, socket bytes -> WS binary.
/// A close/EOF on either side tears down the other.
async fn proxy(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    uds: tokio::net::UnixStream,
) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (mut uds_rx, mut uds_tx) = uds.into_split();

    let to_ws = async {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = uds_rx.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            ws_tx.send(Message::Binary(buf[..n].to_vec())).await?;
        }
        ws_tx.close().await.ok();
        Ok::<(), anyhow::Error>(())
    };

    let to_uds = async {
        while let Some(msg) = ws_rx.next().await {
            match msg? {
                Message::Binary(data) => uds_tx.write_all(&data).await?,
                Message::Text(t) => uds_tx.write_all(t.as_bytes()).await?,
                Message::Close(_) => break,
                // tungstenite answers pings itself; nothing else is data.
                _ => {}
            }
        }
        uds_tx.shutdown().await.ok();
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        r = to_ws => r,
        r = to_uds => r,
    }
}

/// Minimal `/healthz`: 200 once a connection has been served, else 503.
async fn serve_health(port: u16, ready: Arc<AtomicBool>) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, "healthz bind failed");
            return;
        }
    };
    loop {
        let Ok((mut conn, _)) = listener.accept().await else {
            continue;
        };
        let ready = ready.clone();
        tokio::spawn(async move {
            // Read (and discard) the request so the client sees a complete
            // exchange, then reply.
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf).await;
            let (status, body) = if ready.load(Ordering::Relaxed) {
                ("200 OK", "ok")
            } else {
                ("503 Service Unavailable", "not ready")
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = conn.write_all(resp.as_bytes()).await;
            let _ = conn.shutdown().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    // A fake relay: on connect, sends a prologue then echoes what it receives.
    async fn fake_relay(path: std::path::PathBuf, prologue: Vec<u8>) {
        let listener = UnixListener::bind(&path).unwrap();
        let (mut sock, _) = listener.accept().await.unwrap();
        sock.write_all(&prologue).await.unwrap();
        let mut buf = [0u8; 1024];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            sock.write_all(&buf[..n]).await.unwrap();
        }
    }

    #[tokio::test]
    async fn proxies_prologue_and_echoes_round_trip() {
        let dir = std::env::temp_dir().join(format!("msb-bridge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("relay.sock");
        let _ = std::fs::remove_file(&sock_path);

        // Prologue the relay sends first: [id_start BE][id_end BE][ready...].
        let prologue = {
            let mut p = Vec::new();
            p.extend_from_slice(&1u32.to_be_bytes());
            p.extend_from_slice(&100u32.to_be_bytes());
            p.extend_from_slice(b"core.ready");
            p
        };
        tokio::spawn(fake_relay(sock_path.clone(), prologue.clone()));

        // The bridge's WS listener + one connection through the real handle_conn.
        let ws_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = ws_listener.local_addr().unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        {
            let sock_path = sock_path.clone();
            let ready = ready.clone();
            tokio::spawn(async move {
                let (tcp, _) = ws_listener.accept().await.unwrap();
                handle_conn(tcp, &sock_path, Duration::from_secs(5), &ready)
                    .await
                    .unwrap();
            });
        }

        // A real WS client.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();

        // First message is the relay's prologue, forwarded verbatim.
        let first = ws.next().await.unwrap().unwrap();
        assert_eq!(first.into_data(), prologue);

        // A frame we send is echoed back.
        ws.send(Message::Binary(b"hello".to_vec())).await.unwrap();
        let echoed = ws.next().await.unwrap().unwrap();
        assert_eq!(echoed.into_data(), b"hello");

        assert!(ready.load(Ordering::Relaxed));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
