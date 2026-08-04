//! The lifecycle REST routes the upstream `CloudBackend` calls
//! (create/get/list/delete/stop/start), translating wire types <-> the `Sandbox` CRD.

pub mod convert;
mod create;
mod handlers;

use axum::Router;
use axum::routing::{get, post};

use crate::AppState;

/// The lifecycle REST routes, merged into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/sandboxes", post(create::create).get(handlers::list))
        .route(
            "/v1/sandboxes/by-name/{name}",
            get(handlers::get).delete(handlers::delete),
        )
        .route("/v1/sandboxes/by-name/{name}/start", post(handlers::start))
        .route("/v1/sandboxes/by-name/{name}/stop", post(handlers::stop))
        .route("/v1/sandboxes/{name}/logs", get(crate::logs::logs_handler))
}
