//! Device bearer tokens (docs/decisions/auth.md D1, agent.md D4).
//!
//! Enrollment issues ONE device token; that single credential
//! authenticates every device-facing surface (device API, manifest poll,
//! /v2 pulls, websocket, secrets resolve). Tokens are random 256-bit
//! values presented as `Authorization: Bearer rvd_<64 hex>`.
//!
//! Storage: the server persists only the SHA-256 hash of the token.
//! Plain (unsalted, unstretched) SHA-256 is sufficient here because the
//! token is a uniform random 256-bit value — brute force and rainbow
//! tables are hopeless against that entropy; argon2 is for low-entropy
//! human passwords (reeve-server's users table), not machine credentials.
//!
//! spec/reeve/01-framework.md §3.8 item 2 records this as the deliberate
//! v1 replacement of Margo's X.509 + RFC 9421 device auth.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest as _, Sha256};

use crate::identity::Identity;

/// Prefix on every device token, so a leaked credential is recognizable
/// in logs/scanners without being guessable.
pub const DEVICE_TOKEN_PREFIX: &str = "rvd_";

/// Generate a fresh device token: `rvd_` + 64 lowercase hex chars
/// (256 bits from the OS CSPRNG).
///
/// Panics only if OS randomness is unavailable, which is fatal anyway.
pub fn generate_device_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    format!("{DEVICE_TOKEN_PREFIX}{}", hex::encode(buf))
}

/// The stored form of a token: lowercase hex SHA-256 of the full token
/// string (prefix included).
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[derive(Debug, thiserror::Error)]
#[error("device token store: {0}")]
pub struct TokenStoreError(pub String);

/// Lookup seam the middleware authenticates against. reeve-server
/// implements this over its `device_tokens` table; tests use an in-memory
/// map. Law 2: device-api stands alone — no SQLite here.
pub trait DeviceTokenStore: Send + Sync {
    /// Resolve a token hash (as produced by [`token_hash`]) to the id of
    /// an active (non-revoked) device. `Ok(None)` = unknown or revoked.
    fn device_id_for_hash(&self, token_hash: &str) -> Result<Option<String>, TokenStoreError>;
}

/// Tower middleware for device-facing routes: resolves the bearer token
/// into `Identity::Device` or rejects with 401. Mount with
/// `axum::middleware::from_fn_with_state(store, device_auth)`.
///
/// Handlers behind this layer extract [`crate::DeviceIdentity`]; they
/// never see the credential (D1).
pub async fn device_auth(
    State(store): State<Arc<dyn DeviceTokenStore>>,
    mut req: Request,
    next: Next,
) -> Response {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")));

    let Some(token) = bearer else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    match store.device_id_for_hash(&token_hash(token)) {
        Ok(Some(device_id)) => {
            req.extensions_mut().insert(Identity::Device { device_id });
            next.run(req).await
        }
        Ok(None) => StatusCode::UNAUTHORIZED.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "device token lookup failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::DeviceIdentity;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use http_body_util::BodyExt as _;
    use std::collections::HashMap;
    use tower::ServiceExt as _;

    struct MapStore(HashMap<String, String>);

    impl DeviceTokenStore for MapStore {
        fn device_id_for_hash(&self, hash: &str) -> Result<Option<String>, TokenStoreError> {
            Ok(self.0.get(hash).cloned())
        }
    }

    struct FailStore;

    impl DeviceTokenStore for FailStore {
        fn device_id_for_hash(&self, _: &str) -> Result<Option<String>, TokenStoreError> {
            Err(TokenStoreError("db down".into()))
        }
    }

    async fn whoami(DeviceIdentity(id): DeviceIdentity) -> String {
        id
    }

    fn app(store: Arc<dyn DeviceTokenStore>) -> Router {
        Router::new()
            .route("/whoami", get(whoami))
            .layer(axum::middleware::from_fn_with_state(store, device_auth))
    }

    fn req(auth: Option<&str>) -> HttpRequest<Body> {
        let mut b = HttpRequest::builder().uri("/whoami");
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn token_shape_and_hash() {
        let t = generate_device_token();
        assert!(t.starts_with(DEVICE_TOKEN_PREFIX));
        assert_eq!(t.len(), DEVICE_TOKEN_PREFIX.len() + 64);
        assert_ne!(t, generate_device_token(), "tokens must be random");
        // hash is deterministic, hex sha256
        assert_eq!(token_hash(&t), token_hash(&t));
        assert_eq!(token_hash(&t).len(), 64);
        assert_ne!(token_hash(&t), token_hash("other"));
    }

    #[tokio::test]
    async fn valid_token_yields_device_identity() {
        let token = generate_device_token();
        let store = Arc::new(MapStore(HashMap::from([(
            token_hash(&token),
            "dev-42".to_string(),
        )])));
        let res = app(store)
            .oneshot(req(Some(&format!("Bearer {token}"))))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"dev-42");
    }

    #[tokio::test]
    async fn wrong_token_is_401() {
        let token = generate_device_token();
        let store = Arc::new(MapStore(HashMap::from([(
            token_hash(&token),
            "dev-42".to_string(),
        )])));
        let wrong = generate_device_token();
        let res = app(store)
            .oneshot(req(Some(&format!("Bearer {wrong}"))))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_token_is_401() {
        let store = Arc::new(MapStore(HashMap::new()));
        let res = app(store).oneshot(req(None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_bearer_scheme_is_401() {
        let store = Arc::new(MapStore(HashMap::new()));
        let res = app(store)
            .oneshot(req(Some("Basic dXNlcjpwdw==")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn store_error_is_500() {
        let res = app(Arc::new(FailStore))
            .oneshot(req(Some(&format!("Bearer {}", generate_device_token()))))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn unauthenticated_route_without_layer_fails_closed() {
        // Identity extractor with no middleware => 401, never a panic.
        let app = Router::new().route("/whoami", get(whoami));
        let res = app.oneshot(req(None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
