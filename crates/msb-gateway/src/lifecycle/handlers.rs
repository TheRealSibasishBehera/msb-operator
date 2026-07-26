//! The lifecycle REST handlers: get / list / delete / stop / start. Each auths,
//! performs the CRD op, and maps to the cloud wire type.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use kube::api::{DeleteParams, ListParams};
use kube::Api;
use microsandbox_types::{CloudMessageResponse, CloudPaginated};
use msb_crd::Sandbox;

use crate::auth::{self, Identity};
use crate::error::GatewayError;
use crate::lifecycle::convert;
use crate::AppState;

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

/// `GET /v1/sandboxes` — every item must map cleanly or the SDK's page decode fails.
pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let id = match authed(&state, &headers, "list", None).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    match api(&state, &id.namespace).list(&ListParams::default()).await {
        Ok(list) => {
            let data = list
                .items
                .iter()
                .map(|sb| convert::sandbox_to_cloud(sb, &id.namespace))
                .collect::<Vec<_>>();
            // V1: one page, no cursor.
            Json(CloudPaginated { data, next_cursor: None }).into_response()
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
    match api(&state, &id.namespace).delete(&name, &DeleteParams::default()).await {
        Ok(_) => Json(CloudMessageResponse {
            message: format!("sandbox {name} deleted"),
        })
        .into_response(),
        Err(e) => map_kube(&name, e).into_response(),
    }
}

/// `POST /v1/sandboxes/by-name/:name/stop` — V1 stop is delete.
pub async fn stop(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let id = match authed(&state, &headers, "delete", Some(&name)).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let api = api(&state, &id.namespace);
    // Capture before deleting so we can return a coherent CloudSandbox.
    let sb = match api.get(&name).await {
        Ok(sb) => sb,
        Err(e) => return map_kube(&name, e).into_response(),
    };
    if let Err(e) = api.delete(&name, &DeleteParams::default()).await {
        return map_kube(&name, e).into_response();
    }
    // Report it as Stopping regardless of its pre-delete phase.
    let mut cloud = convert::sandbox_to_cloud(&sb, &id.namespace);
    cloud.status = microsandbox_types::CloudSandboxStatus::Stopping;
    Json(cloud).into_response()
}

/// `POST /v1/sandboxes/by-name/:name/start` — no-op; our sandboxes auto-start.
pub async fn start(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    // A no-op read of current state; authorize as `get`.
    let id = match authed(&state, &headers, "get", Some(&name)).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    match api(&state, &id.namespace).get(&name).await {
        Ok(sb) => Json(convert::sandbox_to_cloud(&sb, &id.namespace)).into_response(),
        Err(e) => map_kube(&name, e).into_response(),
    }
}
