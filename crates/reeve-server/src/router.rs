//! Router assembly. One listening socket; /healthz outside auth; human
//! routes behind the D1 identity middleware; the enrollment route
//! (device-api, D4) is authenticated by the join token in its body.
//! Further device routes (C5) nest here later behind
//! `device_api::device_auth`.

use std::sync::Arc;

use axum::routing::{delete, get, post, put};
use axum::{Json, Router, middleware};
use serde_json::json;

use crate::auth;
use crate::enroll::SqliteEnrollmentService;
use crate::join_tokens;
use crate::state::AppState;
use crate::tree;

pub fn build(state: AppState) -> Router {
    let human = Router::new()
        .route("/api/auth/login", post(auth::routes::login))
        .route("/api/auth/logout", post(auth::routes::logout))
        .route("/api/auth/setup", post(auth::routes::setup))
        .route("/api/auth/me", get(auth::routes::me))
        // Join-token management (D4): operator surface, admin/operator
        // role enforced inside the handlers.
        .route(
            "/api/join-tokens",
            post(join_tokens::create).get(join_tokens::index),
        )
        .route("/api/join-tokens/{token_hash}", delete(join_tokens::delete))
        // Tree authoring + inspection (D14): writes operator+, reads
        // viewer+, role enforced inside the handlers; ownership per
        // federation §8.2/§8.4 enforced structurally in tree.rs.
        .route("/api/tree/layers/{layer}", put(tree::put_layer))
        .route(
            "/api/tree/packages/{name}/{version}",
            put(tree::put_package),
        )
        .route("/api/tree/revisions", get(tree::list_revisions))
        .route("/api/tree/revisions/{id}", get(tree::get_revision))
        .route("/api/tree/revisions/{id}/files/{*path}", get(tree::file_at))
        .route("/api/tree/diff/{a}/{b}", get(tree::diff))
        .route("/api/tree/blame/{*path}", get(tree::blame))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::human_auth,
        ));

    // Device-facing enrollment (D4; spec/reeve/01-framework.md §3.8
    // item 1): POST /api/reeve/v1/enroll, no device_auth layer — the
    // join token in the body is the credential.
    let enroll_svc: Arc<dyn device_api::EnrollmentService> = Arc::new(
        SqliteEnrollmentService::new(state.db.clone(), state.revisions.clone()),
    );

    Router::new()
        .merge(human)
        // Operational contract (CLAUDE.md): /healthz, no auth.
        .route("/healthz", get(healthz))
        .with_state(state)
        .merge(device_api::enroll::router(enroll_svc))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}
