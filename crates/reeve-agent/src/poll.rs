//! The manifest poll step — conditional GET, anti-rollback, Law 5.
//!
//! Normative rules implemented here (spec/reeve/08-packaging.md
//! §10.2; docs/decisions/delivery.md D13):
//! - `If-None-Match` with the last-accepted manifest digest; 304 is
//!   a silent no-op.
//! - `manifestVersion` STRICT monotonicity vs the persisted
//!   last-accepted value: a non-increasing value is rejected, logged
//!   as a SECURITY event, and the agent continues from last known
//!   state.
//! - An increase that bumps the epoch bits is accepted and logged as
//!   a NOTABLE event (a restore happened,
//!   spec/reeve/07-durability.md §9.5 restore fencing).
//! - Every network failure logs and continues (Law 5). The poll step
//!   NEVER returns an error that would stop the loop.

use reeve_types::reeve::manifest::{ManifestVersion, StateManifest, is_sha256_digest};
use tracing::{error, info, warn};

use crate::source::{ManifestSource, PollResponse, SourceError};
use crate::state::{AgentDb, Severity};

/// The pure anti-rollback decision (table-tested below): what to do
/// with a received `manifestVersion` given the persisted
/// last-accepted one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionDecision {
    /// No prior accepted manifest — accept and set the floor.
    AcceptFirst,
    /// Strictly greater than the floor. `epoch_bump` = the epoch
    /// bits increased (restore fencing fired) => NOTABLE event.
    Accept { epoch_bump: bool },
    /// Non-increasing (equal or less) => SECURITY event, continue
    /// from last known state.
    RejectNonIncrease,
    /// Outside Margo's modeled range — `ManifestVersion` is in
    /// `[1, 2^64-1]` ("The first manifest MUST use 1",
    /// workload-management-api-1.0.0.yaml), so 0 is never valid.
    RejectInvalid,
}

/// Decide acceptance of `received` against the persisted floor.
/// Pure — the whole §10.2 anti-rollback rule in one testable spot.
pub fn evaluate_version(
    last: Option<ManifestVersion>,
    received: ManifestVersion,
) -> VersionDecision {
    if received == ManifestVersion(0) {
        return VersionDecision::RejectInvalid;
    }
    match last {
        None => VersionDecision::AcceptFirst,
        Some(floor) => {
            if !floor.accepts_successor(received) {
                VersionDecision::RejectNonIncrease
            } else {
                VersionDecision::Accept {
                    epoch_bump: floor.is_epoch_bump(received),
                }
            }
        }
    }
}

/// Outcome of one poll step. Informational — the loop continues
/// identically after every variant (Law 5).
#[derive(Debug)]
pub enum PollOutcome {
    /// 304 / unchanged digest: desired state is current.
    NotModified,
    /// Source unreachable or protocol error — logged; the agent
    /// continues from last known (applied) state.
    SourceUnavailable,
    /// New manifest accepted and persisted as the new floor.
    Accepted {
        manifest: StateManifest,
        etag: String,
        epoch_bump: bool,
    },
    /// Manifest rejected (anti-rollback or invalid); floor unchanged.
    Rejected { received: ManifestVersion },
}

/// One poll: conditional GET against `source`, anti-rollback against
/// the floor persisted in `db`, atomic accept. Infallible by design —
/// every failure path is a logged continue (Law 5); the only
/// panicking condition is a corrupt local database, which recovery
/// cannot paper over.
pub async fn poll_once(db: &mut AgentDb, source: &ManifestSource) -> PollOutcome {
    let last = match db.last_accepted() {
        Ok(l) => l,
        Err(e) => {
            // Local DB unreadable is not an offline condition; still
            // never kill the loop — log and skip this cycle.
            error!(error = %e, "agent.db read failed; skipping poll cycle");
            return PollOutcome::SourceUnavailable;
        }
    };
    let (last_version, last_etag) = match &last {
        Some(a) => (Some(a.version), Some(a.etag.as_str())),
        None => (None, None),
    };

    let response = match source.poll_manifest(last_etag).await {
        Ok(r) => r,
        Err(SourceError::Unreachable(msg)) => {
            // Expected operation for an offline-first agent (Law 5):
            // log, continue from last known state.
            info!(reason = %msg, "manifest source unreachable; continuing from last known state");
            let _ = db.journal(Severity::Info, "poll-unreachable", &msg);
            return PollOutcome::SourceUnavailable;
        }
        Err(SourceError::Protocol(msg)) => {
            warn!(reason = %msg, "manifest poll protocol error; continuing from last known state");
            let _ = db.journal(Severity::Error, "poll-protocol-error", &msg);
            return PollOutcome::SourceUnavailable;
        }
    };

    let (manifest, etag) = match response {
        PollResponse::NotModified => return PollOutcome::NotModified,
        PollResponse::Manifest { manifest, etag } => (manifest, etag),
    };
    let received = manifest.manifest_version;

    // A bundle digest that can't even satisfy the grammar can never
    // verify after pull (§10.2: verify digest) — reject up front.
    if let Some(bundle) = &manifest.bundle
        && !is_sha256_digest(&bundle.digest)
    {
        let msg = format!("bundle digest {:?} violates sha256:<hex> grammar", bundle.digest);
        warn!(%msg, "rejecting manifest");
        let _ = db.journal(Severity::Error, "manifest-bad-digest", &msg);
        return PollOutcome::Rejected { received };
    }

    match evaluate_version(last_version, received) {
        VersionDecision::RejectInvalid => {
            let msg = format!("manifestVersion {} outside valid range [1, 2^64-1]", received.0);
            warn!(%msg, "rejecting manifest");
            let _ = db.journal(Severity::Security, "manifest-invalid-version", &msg);
            PollOutcome::Rejected { received }
        }
        VersionDecision::RejectNonIncrease => {
            // §10.2: SECURITY event; continue from last known state.
            let floor = last_version.expect("RejectNonIncrease implies a floor");
            let msg = format!(
                "manifestVersion did not increase: received {} (epoch {}, counter {}), floor {} (epoch {}, counter {})",
                received.0,
                received.epoch(),
                received.counter(),
                floor.0,
                floor.epoch(),
                floor.counter(),
            );
            warn!(security = true, %msg, "rejecting manifest (anti-rollback)");
            let _ = db.journal(Severity::Security, "manifest-regression", &msg);
            PollOutcome::Rejected { received }
        }
        decision @ (VersionDecision::AcceptFirst | VersionDecision::Accept { .. }) => {
            let epoch_bump = matches!(decision, VersionDecision::Accept { epoch_bump: true });
            let (severity, event, detail) = if epoch_bump {
                // §10.2: epoch bump accepted, NOTABLE event (a
                // restore happened, 07-durability §9.5).
                let floor = last_version.expect("epoch bump implies a floor");
                (
                    Severity::Notable,
                    "manifest-epoch-bump",
                    format!(
                        "epoch bumped {} -> {} (server restore); accepted manifestVersion {}",
                        floor.epoch(),
                        received.epoch(),
                        received.0
                    ),
                )
            } else {
                (
                    Severity::Info,
                    "manifest-accepted",
                    format!("accepted manifestVersion {} (etag {etag})", received.0),
                )
            };
            if epoch_bump {
                warn!(notable = true, detail = %detail, "epoch bump accepted");
            } else {
                info!(detail = %detail, "manifest accepted");
            }
            match db.record_accepted(&manifest, &etag, severity, event, &detail) {
                Ok(()) => PollOutcome::Accepted { manifest, etag, epoch_bump },
                Err(e) => {
                    // Not persisted => not accepted: the floor is the
                    // durable value, so report unavailability and let
                    // the next cycle retry (crash-only: acceptance is
                    // the transaction, nothing lives only in RAM).
                    error!(error = %e, "failed to persist accepted manifest; will retry");
                    PollOutcome::SourceUnavailable
                }
            }
        }
    }
}

/// Convenience for callers/tests: is this a state the agent should
/// hand to the converge step?
impl PollOutcome {
    pub fn accepted(&self) -> Option<&StateManifest> {
        match self {
            PollOutcome::Accepted { manifest, .. } => Some(manifest),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(epoch: u16, counter: u64) -> ManifestVersion {
        ManifestVersion::pack(epoch, counter).unwrap()
    }

    /// The §10.2 anti-rollback rule as a table. This table IS the
    /// spec for `evaluate_version`.
    #[test]
    fn monotonicity_table() {
        use VersionDecision::*;
        let cases: &[(Option<ManifestVersion>, ManifestVersion, VersionDecision)] = &[
            // first manifest: Margo says first MUST be 1; any valid
            // nonzero value sets the floor
            (None, ManifestVersion(1), AcceptFirst),
            (None, v(0, 42), AcceptFirst),
            (None, v(3, 1), AcceptFirst),
            // zero is outside [1, 2^64-1] always
            (None, ManifestVersion(0), RejectInvalid),
            (Some(v(0, 5)), ManifestVersion(0), RejectInvalid),
            // strict increase within an epoch
            (Some(v(0, 5)), v(0, 6), Accept { epoch_bump: false }),
            (Some(v(0, 5)), v(0, 500), Accept { epoch_bump: false }),
            // equal => non-increase => SECURITY
            (Some(v(0, 5)), v(0, 5), RejectNonIncrease),
            // regression => SECURITY
            (Some(v(0, 5)), v(0, 4), RejectNonIncrease),
            (Some(v(0, 5)), v(0, 1), RejectNonIncrease),
            // epoch bump: accepted + NOTABLE, even with counter reset
            (Some(v(0, 5)), v(1, 0), Accept { epoch_bump: true }),
            (Some(v(0, 5)), v(1, 6), Accept { epoch_bump: true }),
            (Some(v(2, 999)), v(3, 0), Accept { epoch_bump: true }),
            // multi-epoch jump is still one bump event
            (Some(v(1, 7)), v(4, 0), Accept { epoch_bump: true }),
            // epoch regression => integer regression => SECURITY
            (Some(v(2, 0)), v(1, u64::MAX >> 16), RejectNonIncrease),
            // top-of-range epochs (sign bit of i64 storage cast)
            (Some(v(0x7FFF, 1)), v(0x8000, 0), Accept { epoch_bump: true }),
            (Some(v(0xFFFF, 1)), v(0xFFFF, 2), Accept { epoch_bump: false }),
            (Some(v(0xFFFF, 2)), v(0xFFFF, 1), RejectNonIncrease),
        ];
        for (last, received, expected) in cases {
            assert_eq!(
                evaluate_version(*last, *received),
                *expected,
                "last={last:?} received={received:?}"
            );
        }
    }
}
