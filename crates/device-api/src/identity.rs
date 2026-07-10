//! The Identity seam (docs/decisions/auth.md D1).
//!
//! All auth is tower middleware + axum extractors: middleware resolves
//! credentials into an [`Identity`] request extension; handlers extract
//! [`Identity`] (or [`DeviceIdentity`]) and NEVER parse credentials
//! themselves. Swapping or adding an auth scheme is one middleware module.
//!
//! spec/reeve/01-framework.md §3.8 item 2: the bearer device credential
//! deliberately replaces Margo's X.509 client certs + HTTP Message
//! Signatures (RFC 9421) in v1. This extractor seam is where certificate/
//! message-signature auth lands later with zero handler changes.

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::{Deserialize, Serialize};

/// Human role (docs/decisions/auth.md D1): admin | operator | viewer.
///
/// Ordered so `Viewer < Operator < Admin`; "at least operator" is
/// `role >= Role::Operator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = UnknownRole;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "viewer" => Ok(Role::Viewer),
            "operator" => Ok(Role::Operator),
            "admin" => Ok(Role::Admin),
            other => Err(UnknownRole(other.to_string())),
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown role {0:?} (expected admin|operator|viewer)")]
pub struct UnknownRole(pub String);

/// Who is making this request (docs/decisions/auth.md D1).
///
/// Inserted into request extensions by auth middleware; never constructed
/// by handlers. `Anonymous` carries no privilege by itself — whether an
/// anonymous request is allowed anything (REEVE_AUTH=none maps Anonymous
/// to admin) is decided by reeve-server's mode-aware authorization, not by
/// this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// An enrolled device authenticated by its enrollment-issued bearer
    /// token (docs/decisions/agent.md D4).
    Device { device_id: String },
    /// A human authenticated by one of the D1 modes (password/proxy).
    Human { user: String, role: Role },
    /// No credential presented (or REEVE_AUTH=none).
    Anonymous,
}

impl Identity {
    /// The device id, if this is a device identity.
    pub fn device_id(&self) -> Option<&str> {
        match self {
            Identity::Device { device_id } => Some(device_id),
            _ => None,
        }
    }

    /// The authenticated human role, if any. `Anonymous` is `None` —
    /// mode-aware elevation (REEVE_AUTH=none) happens in reeve-server.
    pub fn role(&self) -> Option<Role> {
        match self {
            Identity::Human { role, .. } => Some(*role),
            _ => None,
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Identity {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Absence means no auth middleware ran on this route — a router
        // misconfiguration. Fail closed.
        parts
            .extensions
            .get::<Identity>()
            .cloned()
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

/// Extractor for device-only routes: rejects with 401 unless the auth
/// middleware resolved a device identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity(pub String);

impl<S: Send + Sync> FromRequestParts<S> for DeviceIdentity {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Identity::from_request_parts(parts, state).await? {
            Identity::Device { device_id } => Ok(DeviceIdentity(device_id)),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_ordering_admin_top() {
        assert!(Role::Admin > Role::Operator);
        assert!(Role::Operator > Role::Viewer);
        assert!(Role::Operator >= Role::Operator);
    }

    #[test]
    fn role_round_trips_str() {
        for r in [Role::Admin, Role::Operator, Role::Viewer] {
            assert_eq!(r.as_str().parse::<Role>().unwrap(), r);
        }
        assert!("root".parse::<Role>().is_err());
    }

    #[test]
    fn role_serde_lowercase() {
        assert_eq!(serde_json::to_string(&Role::Admin).unwrap(), "\"admin\"");
        assert_eq!(
            serde_json::from_str::<Role>("\"viewer\"").unwrap(),
            Role::Viewer
        );
    }

    #[test]
    fn identity_accessors() {
        let d = Identity::Device {
            device_id: "dev-1".into(),
        };
        assert_eq!(d.device_id(), Some("dev-1"));
        assert_eq!(d.role(), None);

        let h = Identity::Human {
            user: "op".into(),
            role: Role::Operator,
        };
        assert_eq!(h.role(), Some(Role::Operator));
        assert_eq!(h.device_id(), None);

        assert_eq!(Identity::Anonymous.role(), None);
    }
}
