//! Gateway errors, mapped to the typed codes + HTTP status the SDK's CloudBackend
//! recognises (`sandbox_not_found`, `name_already_exists`, `invalid_request`, ...).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// No Sandbox with this name in the gateway's namespace.
    #[error("sandbox not found: {0}")]
    NotFound(String),

    /// The Sandbox exists but has no reachable bridge yet (not Running, or no
    /// serviceName published). Retriable by the client.
    #[error("sandbox not ready: {0}")]
    NotReady(String),

    /// The Bearer token was missing, malformed, or rejected.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The identity is authenticated but not allowed to reach this sandbox.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// Malformed request, invalid name, or a create that reached Failed / timed out.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Create with a name that already exists. The SDK only checks 409 on `POST`.
    #[error("sandbox already exists: {0}")]
    AlreadyExists(String),

    /// A route intentionally unimplemented in V1 (e.g. logs streaming).
    #[error("not implemented: {0}")]
    NotImplemented(String),

    /// Talking to the API server failed.
    #[error(transparent)]
    Kube(#[from] kube::Error),

    /// Dialing or splicing the bridge failed.
    #[error("bridge transport: {0}")]
    Bridge(String),
}

impl GatewayError {
    /// The SDK-facing typed code (mirrors upstream's `CloudErrorBody.code`).
    fn code(&self) -> &'static str {
        match self {
            GatewayError::NotFound(_) => "sandbox_not_found",
            GatewayError::NotReady(_) => "sandbox_not_ready",
            GatewayError::Unauthorized(_) => "unauthorized",
            GatewayError::Forbidden(_) => "forbidden",
            GatewayError::InvalidRequest(_) => "invalid_request",
            GatewayError::AlreadyExists(_) => "name_already_exists",
            GatewayError::NotImplemented(_) => "invalid_request",
            GatewayError::Kube(_) | GatewayError::Bridge(_) => "internal",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            GatewayError::NotFound(_) => StatusCode::NOT_FOUND,
            // 409: exists but not yet execable — the client may retry.
            GatewayError::NotReady(_) => StatusCode::CONFLICT,
            GatewayError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            GatewayError::Forbidden(_) => StatusCode::FORBIDDEN,
            GatewayError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            GatewayError::AlreadyExists(_) => StatusCode::CONFLICT,
            GatewayError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            GatewayError::Kube(_) | GatewayError::Bridge(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "code": self.code(),
            "message": self.to_string(),
        }));
        (self.status(), body).into_response()
    }
}
