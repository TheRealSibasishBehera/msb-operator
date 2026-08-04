//! The lifecycle REST handlers: get / list / delete / stop / start. Each auths,
//! performs the CRD op, and maps to the cloud wire type.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use microsandbox_types::{CloudMessageResponse, CloudPaginated};
use msb_crd::Sandbox;
use serde::Deserialize;

use crate::AppState;
use crate::auth::{self, Identity};
use crate::error::GatewayError;
use crate::lifecycle::convert;

// bearer -> identity (namespace derived from the token) -> SAR for `verb`.
async fn authed(
    state: &AppState,
    headers: &HeaderMap,
    verb: &str,
    name: Option<&str>,
) -> Result<Identity, GatewayError> {
    let token = auth::bearer_token(auth::auth_header(headers))?;
    let id = auth::authenticate_identity(&state.client, &token).await?;
    auth::authorize_verb(&state.client, &id, verb, name).await?;
    Ok(id)
}

fn api(state: &AppState, ns: &str) -> Api<Sandbox> {
    Api::namespaced(state.client.clone(), ns)
}

// A 404 from the API server means the sandbox doesn't exist -> typed not-found.
fn map_kube(name: &str, e: kube::Error) -> GatewayError {
    match &e {
        kube::Error::Api(ae) if ae.code == 404 => GatewayError::NotFound(name.to_string()),
        _ => GatewayError::Kube(e),
    }
}

/// `GET /v1/sandboxes/by-name/:name`
pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let id = match authed(&state, &headers, "get", Some(&name)).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    match api(&state, &id.namespace).get(&name).await {
        Ok(sb) => Json(convert::sandbox_to_cloud(&sb, &id.namespace)).into_response(),
        Err(e) => map_kube(&name, e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub labels: Option<String>,
}

/// `GET /v1/sandboxes[?labels=...]` — every item must map cleanly or the SDK's page
/// decode fails. A `labels` filter is honored as a k8s label selector.
pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let id = match authed(&state, &headers, "list", None).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let mut params = ListParams::default();
    match convert::labels_query_to_selector(q.labels.as_deref()) {
        Ok(Some(sel)) => params = params.labels(&sel),
        Ok(None) => {}
        Err(e) => return e.into_response(),
    }
    match api(&state, &id.namespace).list(&params).await {
        Ok(list) => {
            let data = list
                .items
                .iter()
                .map(|sb| convert::sandbox_to_cloud(sb, &id.namespace))
                .collect::<Vec<_>>();
            // V1: one page, no cursor.
            Json(CloudPaginated {
                data,
                next_cursor: None,
            })
            .into_response()
        }
        Err(e) => GatewayError::Kube(e).into_response(),
    }
}

/// `DELETE /v1/sandboxes/by-name/:name`
pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let id = match authed(&state, &headers, "delete", Some(&name)).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    match api(&state, &id.namespace)
        .delete(&name, &DeleteParams::default())
        .await
    {
        Ok(_) => Json(CloudMessageResponse {
            message: format!("sandbox {name} deleted"),
        })
        .into_response(),
        Err(e) => map_kube(&name, e).into_response(),
    }
}

/// `POST /v1/sandboxes/by-name/:name/stop` — set `spec.desiredState: Stopped`; the
/// Sandbox persists and is restartable.
pub async fn stop(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    set_desired_state(state, name, headers, "Stopped").await
}

/// `POST /v1/sandboxes/by-name/:name/start` — set `spec.desiredState: Running`.
pub async fn start(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    set_desired_state(state, name, headers, "Running").await
}

async fn set_desired_state(
    state: AppState,
    name: String,
    headers: HeaderMap,
    desired: &str,
) -> Response {
    // Editing the spec is an update; authorize the SDK's start/stop as `patch`.
    let id = match authed(&state, &headers, "patch", Some(&name)).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let api = api(&state, &id.namespace);
    let patch = serde_json::json!({ "spec": { "desiredState": desired } });
    match api
        .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(sb) => Json(convert::sandbox_to_cloud(&sb, &id.namespace)).into_response(),
        Err(e) => map_kube(&name, e).into_response(),
    }
}
