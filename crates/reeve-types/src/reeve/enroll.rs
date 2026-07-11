//! Enrollment wire types (docs/decisions/agent.md D4;
//! spec/reeve/01-framework.md §3.8 item 1).
//!
//! `POST /api/reeve/v1/enroll` is the reeve replacement for Margo's
//! onboarding & certificate API — a reeve surface (§3.1 rule 4), never
//! under Margo's `/api/v1/`. Field names are exactly as recorded in D4
//! step 1 (snake_case).
//!
//! Shared by device-api (serves the route) and reeve-agent (calls it);
//! serde only — no I/O in this crate.

use serde::{Deserialize, Serialize};

/// Request body of `POST /api/reeve/v1/enroll` (D4 step 1):
/// `{ join_token, hostname, arch, agent_version }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EnrollRequest {
    /// Operator-created join token (D4: TTL + max-uses, stored hashed
    /// server-side). A re-enroll token is the same grammar, bound to an
    /// existing device_id server-side.
    pub join_token: String,
    /// The enrolling box's hostname — the idempotency key for a retried
    /// install with the same join token (D4).
    pub hostname: String,
    /// Target architecture (e.g. `x86_64`, `aarch64`).
    pub arch: String,
    /// reeve-agent version performing the enrollment.
    pub agent_version: String,
}

/// Response of `POST /api/reeve/v1/enroll` (D4 step 2): the issued
/// identity and the ONE device credential for every device-facing
/// surface (docs/decisions/auth.md D1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EnrollResponse {
    /// Server-issued device identity.
    pub device_id: String,
    /// Enrollment-issued bearer token (`rvd_<64 hex>`). Shown exactly
    /// once; the server keeps only its hash.
    pub device_token: String,
    /// True when this enrollment resumed an existing device identity
    /// (idempotent re-run of the same join token + hostname, or a
    /// re-enroll token bound to an existing device — D4).
    #[serde(default)]
    pub resumed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_d4_field_names() {
        let json = r#"{
            "join_token": "rvj_abc",
            "hostname": "edge-01",
            "arch": "x86_64",
            "agent_version": "0.1.0"
        }"#;
        let req: EnrollRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.join_token, "rvj_abc");
        assert_eq!(req.hostname, "edge-01");
        let back = serde_json::to_value(&req).unwrap();
        assert_eq!(back["join_token"], "rvj_abc");
        assert_eq!(back["agent_version"], "0.1.0");
    }

    #[test]
    fn response_resumed_defaults_false_and_unknown_fields_tolerated() {
        // §3.6: unknown-field tolerant, no deny_unknown_fields.
        let json = r#"{"device_id":"dev-1","device_token":"rvd_x","future":1}"#;
        let resp: EnrollResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.device_id, "dev-1");
        assert!(!resp.resumed);
    }
}
