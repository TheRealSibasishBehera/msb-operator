//! `POST /v1/sandboxes[?start=true]` — the one route with real logic.
//!
//! Create the Sandbox CRD, then watch until it reaches `Running` within the
//! bounded budget (< the SDK's 30s client timeout). Outcomes:
//!   - Running within budget → 200 `CloudSandbox`.
//!   - Failed first          → delete the CRD, 400 `invalid_request` (+reason).
//!   - neither within budget → delete the CRD, 400 `invalid_request`.
//!   - name already exists   → 409 `name_already_exists` (POST only).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use kube::api::{DeleteParams, ObjectMeta, PostParams};
use kube::Api;
use microsandbox_types::CloudCreateSandboxRequest;
use msb_crd::{Sandbox, SandboxPhase};
use serde::Deserialize;
use tracing::warn;

use crate::error::GatewayError;
use crate::lifecycle::convert;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateQuery {
    #[serde(default)]
    pub start: bool,
}

/// Poll interval while waiting for Running.
const POLL: Duration = Duration::from_millis(500);

pub async fn create(
    State(state): State<AppState>,
    Query(q): Query<CreateQuery>,
    headers: HeaderMap,
    Json(req): Json<CloudCreateSandboxRequest>,
) -> Response {
    // Our sandboxes auto-start on create; `?start=false` is accepted but has no
    // distinct "created-but-stopped" state in V1 (we still create + wait). This is
    // a deliberate V1 limitation, surfaced at `warn` so the deviation is visible:
    // a caller relying on `start=false` will still get a Running sandbox back.
    if !q.start {
        tracing::warn!(sandbox = %req.name, "create with start=false; V1 boots the sandbox anyway");
    }
    // Auth: authenticate + derive namespace + authorize `create`.
    let token = match crate::auth::bearer_token(crate::auth::auth_header(&headers)) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    let id = match crate::auth::authenticate_identity(&state.client, &token).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = crate::auth::authorize_verb(&state.client, &id, "create", None).await {
        return e.into_response();
    }

    // Inbound map (validates the name) → spec + annotations.
    let (spec, ann) = match convert::request_to_spec(&req) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

    let api: Api<Sandbox> = Api::namespaced(state.client.clone(), &id.namespace);
    let sandbox = Sandbox {
        metadata: ObjectMeta {
            name: Some(req.name.clone()),
            namespace: Some(id.namespace.clone()),
            annotations: if ann.is_empty() {
                None
            } else {
                Some(ann.into_iter().collect::<BTreeMap<_, _>>())
            },
            ..Default::default()
        },
        spec,
        status: None,
    };

    // Create — a k8s AlreadyExists (409) maps to name_already_exists.
    if let Err(e) = api.create(&PostParams::default(), &sandbox).await {
        return match &e {
            kube::Error::Api(ae) if ae.code == 409 => {
                GatewayError::AlreadyExists(req.name.clone()).into_response()
            }
            _ => GatewayError::Kube(e).into_response(),
        };
    }

    // Watch until Running within the budget. Poll loop keeps deps light.
    let deadline = Instant::now() + state.create_timeout;
    loop {
        let sb = match api.get(&req.name).await {
            Ok(sb) => sb,
            Err(e) => return GatewayError::Kube(e).into_response(),
        };
        let phase = sb.status.as_ref().and_then(|s| s.phase.clone());
        match phase {
            Some(SandboxPhase::Running) => {
                return Json(convert::sandbox_to_cloud(&sb, &id.namespace)).into_response();
            }
            Some(SandboxPhase::Failed) => {
                let reason = sb
                    .status
                    .as_ref()
                    .and_then(|s| s.termination_reason.as_ref())
                    .map(|r| format!("{r:?}"))
                    .unwrap_or_else(|| "sandbox failed".into());
                cleanup(&api, &req.name).await;
                return GatewayError::InvalidRequest(format!(
                    "sandbox {} reached Failed before Running: {reason}",
                    req.name
                ))
                .into_response();
            }
            // Succeeded before we observed Running: a very fast run-to-completion.
            // Treat as success and return it (SDK sees Stopped).
            Some(SandboxPhase::Succeeded) => {
                return Json(convert::sandbox_to_cloud(&sb, &id.namespace)).into_response();
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            warn!(sandbox = %req.name, "create did not reach Running within budget");
            cleanup(&api, &req.name).await;
            return GatewayError::InvalidRequest(format!(
                "sandbox {} did not reach Running within {}s — raise the SDK request_timeout \
                 and retry, or the image is slow to boot",
                req.name,
                state.create_timeout.as_secs()
            ))
            .into_response();
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Best-effort delete of a half-born sandbox(no orphaned Pending objects).
async fn cleanup(api: &Api<Sandbox>, name: &str) {
    if let Err(e) = api.delete(name, &DeleteParams::default()).await {
        warn!(sandbox = %name, error = %e, "failed to clean up sandbox after create failure");
    }
}
