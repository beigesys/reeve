//! Shared application state: config + the single SQLite DB (Law 4).

use std::sync::{Arc, Mutex};

use device_api::{Identity, Role};
use rusqlite::Connection;

use crate::config::{AuthMode, Config};
use crate::ownership::Ownership;

/// Cloneable handle threaded through every route.
///
/// Locking: `db` is THE single writer connection (D6/D16 — server
/// tables AND revision-store tables; the durability changeset session
/// rides it, spec/reeve/07-durability.md §9.3). `revisions` wraps the
/// SAME connection via `RevisionStore::from_shared` and locks `db`
/// internally per call. Lock order: `revisions` may be held while a
/// store method briefly takes `db`; code holding `db` MUST NOT call
/// into `revisions` (one-direction rule — no cycles). Locks are short
/// and never held across `.await`.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Arc<Mutex<Connection>>,
    pub revisions: Arc<Mutex<revision_store::RevisionStore>>,
    /// The C6 durability engine (spec/reeve/07-durability.md §9.1 —
    /// ONE trait seam; tier selected by config).
    pub durability: Arc<dyn crate::durability::Durability>,
    /// True when this boot applied schema migrations — D16: a schema
    /// migration must cut a new snapshot generation (durability::startup
    /// consumes this).
    pub migrated_at_boot: bool,
    /// sha256 hex of the one-time first-boot setup token (password mode,
    /// zero users). In memory only: a crash mints a fresh one on restart
    /// (crash-only — nothing to persist, startup regenerates).
    pub setup_token_hash: Arc<Mutex<Option<String>>>,
    /// Which tree paths this tier may author (federation §8.4 single
    /// writer per layer). v1 single-tier: [`Ownership::Root`]; C10
    /// populates [`Ownership::Gateway`] from tier configuration.
    pub ownership: Arc<Ownership>,
}

impl AppState {
    /// Mode-aware authorization (docs/decisions/auth.md D1): the role this
    /// identity acts with. `Anonymous` is admin ONLY under REEVE_AUTH=none;
    /// devices carry no human role.
    pub fn effective_role(&self, identity: &Identity) -> Option<Role> {
        match identity {
            Identity::Human { role, .. } => Some(*role),
            Identity::Anonymous => match self.cfg.auth {
                AuthMode::None => Some(Role::Admin),
                _ => None,
            },
            Identity::Device { .. } => None,
        }
    }
}
