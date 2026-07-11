//! Device-facing HTTP surface: enrollment and status ingest (routes land
//! with C2/C5), plus the identity/auth seam shared with reeve-server.
//!
//! Placement (CLAUDE.md layout + Law 2): the `Identity` type, its axum
//! extractors, and device-token auth live here so both device-api routes
//! and reeve-server human routes consume ONE seam. Human auth *modes*
//! (password/proxy/none — docs/decisions/auth.md D1) live in reeve-server;
//! this crate defines only the identity vocabulary and the device
//! credential machinery.

pub mod device_token;
pub mod enroll;
pub mod identity;
pub mod status;

/// The one error-body shape every JSON error response uses:
/// `{ "error": "<human-readable message>" }`. Schema component for the
/// D10 openapi.json (docs/decisions/ui.md); handlers keep building the
/// body via `json!` — this type documents the contract.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ErrorBody {
    pub error: String,
}

pub use device_token::{
    DEVICE_TOKEN_PREFIX, DeviceTokenStore, TokenStoreError, device_auth, generate_device_token,
    token_hash,
};
pub use enroll::{ENROLL_PATH, EnrollError, EnrollRequest, EnrollResponse, EnrollmentService};
pub use identity::{DeviceIdentity, Identity, Role};
pub use status::{JOURNAL_ROUTE, MARGO_STATUS_ROUTE, StatusIngest, StatusIngestError};
