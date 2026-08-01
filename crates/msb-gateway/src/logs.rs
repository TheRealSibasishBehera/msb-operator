//! `GET /v1/sandboxes/:name/logs` — streams the sandbox's `msb-console-log`
//! sidecar container log and reformats each JSON line into the msb SDK's
//! cloud-backend SSE contract (`event: log` / `event: end`). Live-follow only
//! for v1: no historical replay, no `since`/`until`.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::io::AsyncBufReadExt as _;
use futures::{Stream, StreamExt};
use kube::api::LogParams;
use kube::Api;
use k8s_openapi::api::core::v1::Pod;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::auth;
use crate::error::GatewayError;
use crate::AppState;

#[derive(Debug, Deserialize)]
struct ConsoleLogLine {
    source: String,
    ts: chrono::DateTime<chrono::Utc>,
    text: String,
}

/// The `data:` payload of an `event: log` SSE frame, as the msb SDK parses it.
#[derive(Serialize)]
struct CloudLogPayload<'a> {
    source: &'a str,
    ts: chrono::DateTime<chrono::Utc>,
    text: &'a str,
}

fn log_event(line: &ConsoleLogLine) -> String {
    let payload = CloudLogPayload {
        source: &line.source,
        ts: line.ts,
        text: &line.text,
    };
    let data = serde_json::to_string(&payload).expect("cloud log payload always serializes");
    format!("event: log\ndata: {data}\n\n")
}

/// The terminal SSE block that ends the SDK's parse loop.
fn end_event() -> &'static str {
    "event: end\n\n"
}

pub async fn logs_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let token = match auth::bearer_token(auth::auth_header(&headers)) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    let id = match auth::authenticate_identity(&state.client, &token).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = auth::authorize_verb(&state.client, &id, "get", Some(&name)).await {
        return e.into_response();
    }

    let sandboxes: Api<msb_crd::Sandbox> = Api::namespaced(state.client.clone(), &id.namespace);
    let sandbox = match sandboxes.get_opt(&name).await {
        Ok(Some(sb)) => sb,
        Ok(None) => return GatewayError::NotFound(name).into_response(),
        Err(e) => return GatewayError::Kube(e).into_response(),
    };

    if !sandbox.spec.logging.guest_console {
        return GatewayError::NotReady(format!(
            "sandbox {name} does not have spec.logging.guestConsole enabled"
        ))
        .into_response();
    }
    let pod_name = match sandbox.status.as_ref().and_then(|s| s.pod_name.as_deref()) {
        Some(p) => p.to_string(),
        None => return GatewayError::NotReady(name).into_response(),
    };

    let pods: Api<Pod> = Api::namespaced(state.client.clone(), &id.namespace);
    let lp = LogParams {
        follow: true,
        container: Some("msb-console-log".to_string()),
        ..Default::default()
    };
    let container_log = match pods.log_stream(&pod_name, &lp).await {
        Ok(s) => s,
        Err(e) => return GatewayError::Kube(e).into_response(),
    };

    let body = Body::from_stream(sse_stream(container_log));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .expect("static headers and a streaming body always build")
}

/// A non-JSON line is skipped, not fatal: the k8s log endpoint can splice partial
/// lines around a container restart.
fn sse_stream(
    reader: impl futures::AsyncBufRead + Send + 'static,
) -> impl Stream<Item = Result<axum::body::Bytes, std::io::Error>> + Send + 'static {
    let lines = reader.lines();
    let events = lines.filter_map(|line| async move {
        match line {
            Ok(raw) => match serde_json::from_str::<ConsoleLogLine>(&raw) {
                Ok(parsed) => Some(Ok(axum::body::Bytes::from(log_event(&parsed)))),
                Err(e) => {
                    warn!(error = %e, line = %raw, "console-log: skipping malformed line");
                    None
                }
            },
            Err(e) => Some(Err(e)),
        }
    });
    events.chain(futures::stream::once(async {
        Ok(axum::body::Bytes::from_static(end_event().as_bytes()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_a_log_line_as_an_sse_log_event() {
        let line = ConsoleLogLine {
            source: "stdout".to_string(),
            ts: "2026-01-01T00:00:00Z".parse().unwrap(),
            text: "hello".to_string(),
        };
        let event = log_event(&line);
        assert!(event.starts_with("event: log\ndata: "));
        assert!(event.ends_with("\n\n"));
        let data_line = event.strip_prefix("event: log\ndata: ").unwrap();
        let data_line = data_line.trim_end_matches("\n\n");
        let v: serde_json::Value = serde_json::from_str(data_line).unwrap();
        assert_eq!(v["source"], "stdout");
        assert_eq!(v["text"], "hello");
        assert_eq!(v["ts"], "2026-01-01T00:00:00Z");
    }

    #[test]
    fn end_event_is_the_bare_terminator() {
        assert_eq!(end_event(), "event: end\n\n");
    }

    #[tokio::test]
    async fn sse_stream_reformats_lines_and_terminates_with_end() {
        let input = "{\"source\":\"stdout\",\"ts\":\"2026-01-01T00:00:00Z\",\"text\":\"a\"}\n{\"source\":\"stderr\",\"ts\":\"2026-01-01T00:00:01Z\",\"text\":\"b\"}\n";
        let reader = futures::io::Cursor::new(input.as_bytes().to_vec());
        let events: Vec<_> = sse_stream(reader).collect().await;
        assert_eq!(events.len(), 3);
        let frame0 = String::from_utf8(events[0].as_ref().unwrap().to_vec()).unwrap();
        assert!(frame0.contains("\"source\":\"stdout\""));
        assert!(frame0.starts_with("event: log\n"));
        let frame2 = String::from_utf8(events[2].as_ref().unwrap().to_vec()).unwrap();
        assert_eq!(frame2, "event: end\n\n");
    }

    #[tokio::test]
    async fn sse_stream_skips_malformed_lines_without_crashing() {
        let input = "not json\n{\"source\":\"stdout\",\"ts\":\"2026-01-01T00:00:00Z\",\"text\":\"ok\"}\n";
        let reader = futures::io::Cursor::new(input.as_bytes().to_vec());
        let events: Vec<_> = sse_stream(reader).collect().await;
        assert_eq!(events.len(), 2);
        let frame0 = String::from_utf8(events[0].as_ref().unwrap().to_vec()).unwrap();
        assert!(frame0.contains("\"text\":\"ok\""));
    }
}
