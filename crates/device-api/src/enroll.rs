//! `POST /api/reeve/v1/enroll` — the device-facing enrollment route
//! (docs/decisions/agent.md D4; spec/reeve/01-framework.md §3.8 item 1:
//! this reeve surface REPLACES Margo's onboarding & certificate API).
//!
//! Placement (Law 2): the route and its wire handling live here because
//! they are device-facing; the persistence (join tokens, device rows,
//! initial desired state) lives behind [`EnrollmentService`], which
//! reeve-server implements over its SQLite DB + revision store. Join
//! *token management* (create/list/revoke) is a human operator surface
//! and lives in reeve-server, not here.
//!
//! This route is NOT behind `device_auth`: the join token in the body IS
//! the credential that bootstraps the device credential.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

pub use reeve_types::reeve::enroll::{EnrollRequest, EnrollResponse};

/// Path of the enrollment route (spec/reeve/01-framework.md §3.8 item 1).
pub const ENROLL_PATH: &str = "/api/reeve/v1/enroll";

/// Why an enrollment was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    /// Unknown, expired, exhausted, or revoked join token. Deliberately
    /// one variant: the response must not disclose WHICH check failed
    /// (a join token probe learns nothing beyond "no").
    #[error("invalid, expired, or exhausted join token")]
    InvalidToken,
    /// Malformed request content (e.g. empty hostname).
    #[error("invalid enrollment request: {0}")]
    Invalid(String),
    /// Persistence failure — the agent should retry; enrollment is
    /// idempotent for the same join token + hostname (D4).
    #[error("enrollment failed: {0}")]
    Internal(String),
}

/// Persistence seam the route calls into. reeve-server implements this
/// over its SQLite DB + revision store; tests use a mock. The whole
/// D4 ceremony (validate token, create/resume device row, initial
/// desired state, issue device token) happens behind this call.
pub trait EnrollmentService: Send + Sync {
    fn enroll(&self, req: &EnrollRequest) -> Result<EnrollResponse, EnrollError>;
}

/// Build the enrollment router. Mount by merging into the server's
/// top-level router (the route path is absolute).
pub fn router(svc: Arc<dyn EnrollmentService>) -> Router {
    Router::new()
        .route(ENROLL_PATH, post(enroll_route))
        .with_state(svc)
}

/// POST /api/reeve/v1/enroll (D4 step 1-2).
#[utoipa::path(
    post,
    path = "/api/reeve/v1/enroll",
    tag = "enroll",
    request_body = EnrollRequest,
    responses(
        (status = 200, description = "Enrolled (or idempotently resumed); the device credential is shown exactly once", body = EnrollResponse),
        (status = 401, description = "Invalid, expired, or exhausted join token", body = crate::ErrorBody),
        (status = 422, description = "Malformed enrollment request", body = crate::ErrorBody),
        (status = 500, description = "Enrollment failed", body = crate::ErrorBody),
    ),
)]
pub async fn enroll_route(
    State(svc): State<Arc<dyn EnrollmentService>>,
    Json(mut req): Json<EnrollRequest>,
) -> Response {
    req.hostname = req.hostname.trim().to_string();
    if req.hostname.is_empty() {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "hostname must be non-empty");
    }
    match svc.enroll(&req) {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(EnrollError::InvalidToken) => error_response(
            StatusCode::UNAUTHORIZED,
            "invalid, expired, or exhausted join token",
        ),
        Err(EnrollError::Invalid(msg)) => error_response(StatusCode::UNPROCESSABLE_ENTITY, &msg),
        Err(EnrollError::Internal(msg)) => {
            tracing::error!(error = %msg, "enrollment failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "enrollment failed")
        }
    }
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    struct OkService;

    impl EnrollmentService for OkService {
        fn enroll(&self, req: &EnrollRequest) -> Result<EnrollResponse, EnrollError> {
            if req.join_token != "rvj_good" {
                return Err(EnrollError::InvalidToken);
            }
            Ok(EnrollResponse {
                device_id: "dev-1".into(),
                device_token: "rvd_tok".into(),
                resumed: false,
            })
        }
    }

    fn post_json(body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(ENROLL_PATH)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn req_body(token: &str, hostname: &str) -> String {
        serde_json::to_string(&EnrollRequest {
            join_token: token.into(),
            hostname: hostname.into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn happy_path_returns_identity_and_token() {
        let app = router(Arc::new(OkService));
        let res = app
            .oneshot(post_json(&req_body("rvj_good", "edge-01")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let resp: EnrollResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(resp.device_id, "dev-1");
        assert_eq!(resp.device_token, "rvd_tok");
    }

    #[tokio::test]
    async fn bad_token_is_401() {
        let app = router(Arc::new(OkService));
        let res = app
            .oneshot(post_json(&req_body("rvj_bad", "edge-01")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_hostname_is_422() {
        let app = router(Arc::new(OkService));
        let res = app
            .oneshot(post_json(&req_body("rvj_good", "   ")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn internal_error_is_500_without_detail() {
        struct FailService;
        impl EnrollmentService for FailService {
            fn enroll(&self, _: &EnrollRequest) -> Result<EnrollResponse, EnrollError> {
                Err(EnrollError::Internal("db down: secret detail".into()))
            }
        }
        let app = router(Arc::new(FailService));
        let res = app
            .oneshot(post_json(&req_body("rvj_good", "edge-01")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!text.contains("secret detail"), "500 body must not leak internals");
    }
}
