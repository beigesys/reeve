//! SQLite implementation of device-api's [`DeviceTokenStore`] plus token
//! issuance/revocation for enrollment (C2, docs/decisions/agent.md D4).
//!
//! One credential per enrollment authenticates every device-facing
//! surface; revocation (set `revoked_at`) is full site cutoff
//! (docs/decisions/auth.md D1).

use std::sync::{Arc, Mutex};

use device_api::{DeviceTokenStore, TokenStoreError, generate_device_token, token_hash};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::db::now_secs;

/// `DeviceTokenStore` over the server's `device_tokens` table.
#[derive(Clone)]
pub struct SqliteDeviceTokenStore {
    db: Arc<Mutex<Connection>>,
}

impl SqliteDeviceTokenStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }
}

impl DeviceTokenStore for SqliteDeviceTokenStore {
    fn device_id_for_hash(&self, hash: &str) -> Result<Option<String>, TokenStoreError> {
        let conn = self.db.lock().expect("db mutex poisoned");
        conn.query_row(
            "SELECT device_id FROM device_tokens
             WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![hash],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| TokenStoreError(e.to_string()))
    }
}

/// Issue a fresh token for `device_id` and store its hash. Returns the raw
/// token — the ONLY time it exists server-side; hand it to the enrolling
/// agent and forget it. Transactional via the single INSERT.
pub fn issue(conn: &Connection, device_id: &str) -> rusqlite::Result<String> {
    let token = generate_device_token();
    conn.execute(
        "INSERT INTO device_tokens (token_hash, device_id, created_at)
         VALUES (?1, ?2, ?3)",
        params![token_hash(&token), device_id, now_secs()],
    )?;
    Ok(token)
}

/// Revoke every active token for `device_id` (D1: one revocation = full
/// site cutoff). Idempotent.
pub fn revoke_all(conn: &Connection, device_id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE device_tokens SET revoked_at = ?1
         WHERE device_id = ?2 AND revoked_at IS NULL",
        params![now_secs(), device_id],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn test_db() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "on").unwrap();
        db::migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO devices (device_id, enrolled_at) VALUES ('dev-1', 0)",
            [],
        )
        .unwrap();
        Arc::new(Mutex::new(conn))
    }

    #[test]
    fn issue_lookup_revoke_round_trip() {
        let db = test_db();
        let token = issue(&db.lock().unwrap(), "dev-1").unwrap();
        let store = SqliteDeviceTokenStore::new(db.clone());

        assert_eq!(
            store.device_id_for_hash(&token_hash(&token)).unwrap(),
            Some("dev-1".to_string())
        );
        assert_eq!(store.device_id_for_hash(&token_hash("rvd_bogus")).unwrap(), None);

        revoke_all(&db.lock().unwrap(), "dev-1").unwrap();
        assert_eq!(
            store.device_id_for_hash(&token_hash(&token)).unwrap(),
            None,
            "revoked token must not authenticate"
        );
    }
}
