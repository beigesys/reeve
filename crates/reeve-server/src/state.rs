//! Shared application state: config + the single SQLite DB (Law 4).

use std::sync::{Arc, Mutex};

use device_api::{Identity, Role};
use rusqlite::Connection;

use crate::config::{AuthMode, Config};
use crate::ownership::Ownership;

/// Cloneable handle threaded through every route.
///
/// Locking: `db` is the server-tables writer connection (D6 single-writer
/// discipline); `revisions` is the revision store's own connection to the
/// same file. Locks are short and never held across `.await`.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Arc<Mutex<Connection>>,
    pub revisions: Arc<Mutex<revision_store::RevisionStore>>,
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
