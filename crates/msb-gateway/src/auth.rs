//! Bearer-token auth reusing Kubernetes identity — no key store of our own.
//!
//! The SDK sends `Authorization: Bearer <MSB_API_KEY>`. We treat that token as a
//! Kubernetes ServiceAccount token: `TokenReview` authenticates it, then
//! `SubjectAccessReview` authorizes "can this identity `get` this Sandbox in this
//! namespace?" — the *same* RBAC gate the human door uses. No issuance, no
//! revocation, no mapping table to secure.

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use kube::api::PostParams;
use kube::{Api, Client};

use crate::error::GatewayError;

const SANDBOX_GROUP: &str = "sandbox.microsandbox.dev";
const SANDBOX_RESOURCE: &str = "sandboxes";

/// An authenticated caller, with the namespace derived from the token (the SDK's
/// request carries none).
#[derive(Debug, Clone)]
pub struct Identity {
    pub username: String,
    pub namespace: String,
}

/// The raw `Authorization` header value, if present and valid UTF-8.
pub fn auth_header(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}

/// Extract the bearer token from an `Authorization` header value.
pub fn bearer_token(header: Option<&str>) -> Result<String, GatewayError> {
    let raw = header.ok_or_else(|| GatewayError::Unauthorized("missing Authorization".into()))?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .ok_or_else(|| GatewayError::Unauthorized("expected Bearer token".into()))?;
    if token.is_empty() {
        return Err(GatewayError::Unauthorized("empty Bearer token".into()));
    }
    Ok(token.to_string())
}

/// Derive the namespace from a ServiceAccount username; non-SA identities can't be
/// served (the SDK conveys no namespace).
pub fn namespace_from_username(username: &str) -> Result<String, GatewayError> {
    // Format: system:serviceaccount:<namespace>:<name>
    let parts: Vec<&str> = username.split(':').collect();
    if parts.len() == 4 && parts[0] == "system" && parts[1] == "serviceaccount" {
        Ok(parts[2].to_string())
    } else {
        Err(GatewayError::InvalidRequest(format!(
            "cannot derive namespace from identity {username:?}: use a ServiceAccount token \
             (the SDK sends no namespace, so it is taken from the SA's namespace)"
        )))
    }
}

/// Authenticate + derive namespace, but authorize nothing — call `authorize_verb`.
pub async fn authenticate_identity(
    client: &Client,
    token: &str,
) -> Result<Identity, GatewayError> {
    let username = authenticate(client, token).await?;
    let namespace = namespace_from_username(&username)?;
    Ok(Identity { username, namespace })
}

/// Full check for the exec path: authenticate, derive namespace, authorize `get`
/// on the named sandbox. Returns the identity (namespace + username).
pub async fn authorize(
    client: &Client,
    token: &str,
    name: &str,
) -> Result<Identity, GatewayError> {
    let id = authenticate_identity(client, token).await?;
    authorize_verb(client, &id, "get", Some(name)).await?;
    Ok(id)
}

/// TokenReview → the authenticated username, or Unauthorized.
async fn authenticate(client: &Client, token: &str) -> Result<String, GatewayError> {
    let api: Api<TokenReview> = Api::all(client.clone());
    let review = TokenReview {
        spec: TokenReviewSpec {
            token: Some(token.to_string()),
            audiences: None,
        },
        ..Default::default()
    };
    let result = api
        .create(&PostParams::default(), &review)
        .await
        .map_err(GatewayError::Kube)?;

    let status = result
        .status
        .ok_or_else(|| GatewayError::Unauthorized("token review returned no status".into()))?;
    if !status.authenticated.unwrap_or(false) {
        let msg = status.error.unwrap_or_else(|| "token not authenticated".into());
        return Err(GatewayError::Unauthorized(msg));
    }
    status
        .user
        .and_then(|u| u.username)
        .ok_or_else(|| GatewayError::Unauthorized("token review returned no username".into()))
}

/// SubjectAccessReview: may this identity do `verb` on `sandboxes` (a
/// specific `name`, or the collection when `name` is None) in its namespace?
/// The verb varies per route: create→`create`, get/list→`get`/`list`,
/// stop/delete→`delete`, exec→`get`.
pub async fn authorize_verb(
    client: &Client,
    id: &Identity,
    verb: &str,
    name: Option<&str>,
) -> Result<(), GatewayError> {
    let api: Api<SubjectAccessReview> = Api::all(client.clone());
    let review = SubjectAccessReview {
        spec: SubjectAccessReviewSpec {
            user: Some(id.username.clone()),
            resource_attributes: Some(ResourceAttributes {
                namespace: Some(id.namespace.clone()),
                verb: Some(verb.to_string()),
                group: Some(SANDBOX_GROUP.to_string()),
                resource: Some(SANDBOX_RESOURCE.to_string()),
                name: name.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let result = api
        .create(&PostParams::default(), &review)
        .await
        .map_err(GatewayError::Kube)?;

    let status = result
        .status
        .ok_or_else(|| GatewayError::Forbidden("access review returned no status".into()))?;
    if status.allowed {
        Ok(())
    } else {
        let reason = status.reason.unwrap_or_else(|| "not allowed".into());
        let target = name.unwrap_or("<collection>");
        Err(GatewayError::Forbidden(format!(
            "{} cannot {verb} sandbox {}/{target}: {reason}",
            id.username, id.namespace
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer() {
        assert_eq!(bearer_token(Some("Bearer abc")).unwrap(), "abc");
        assert_eq!(bearer_token(Some("bearer abc")).unwrap(), "abc");
    }

    #[test]
    fn rejects_missing_or_malformed() {
        assert!(bearer_token(None).is_err());
        assert!(bearer_token(Some("abc")).is_err());
        assert!(bearer_token(Some("Bearer ")).is_err());
        assert!(bearer_token(Some("Basic xyz")).is_err());
    }

    #[test]
    fn derives_namespace_from_sa_username() {
        assert_eq!(
            namespace_from_username("system:serviceaccount:team-a:my-app").unwrap(),
            "team-a"
        );
    }

    #[test]
    fn rejects_non_sa_identity() {
        assert!(namespace_from_username("kubernetes-admin").is_err());
        assert!(namespace_from_username("system:node:worker-1").is_err());
        assert!(matches!(
            namespace_from_username("alice@example.com"),
            Err(GatewayError::InvalidRequest(_))
        ));
    }
}
