//! Local users table (REEVE_AUTH=password, docs/decisions/auth.md D1).
//! Passwords are argon2id (PHC string, argon2 crate defaults).

use std::sync::OnceLock;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use device_api::Role;
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::db::now_secs;

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("password hashing failed: {0}")]
    Hash(String),
}

pub fn hash_password(password: &str) -> Result<String, UserError> {
    // 128-bit random salt straight from the OS CSPRNG (avoids depending on
    // the password-hash crate's rand_core re-export feature set).
    let mut salt_bytes = [0u8; 16];
    getrandom::fill(&mut salt_bytes).expect("OS randomness unavailable");
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| UserError::Hash(e.to_string()))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| UserError::Hash(e.to_string()))
}

/// Dummy hash verified against when the username doesn't exist, so login
/// timing doesn't reveal which usernames are real.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("reeve-dummy-password").expect("argon2 self-hash"))
}

pub fn count(conn: &Connection) -> Result<i64, UserError> {
    Ok(conn.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?)
}

/// Create a user. Fails if the username exists (PRIMARY KEY).
pub fn create(
    conn: &Connection,
    username: &str,
    password: &str,
    role: Role,
) -> Result<(), UserError> {
    let hash = hash_password(password)?;
    conn.execute(
        "INSERT INTO users (username, password_hash, role, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![username, hash, role.as_str(), now_secs()],
    )?;
    Ok(())
}

/// Verify a password. Returns the user's role on success, `None` on any
/// failure (unknown user, wrong password) — indistinguishable to callers.
pub fn verify(conn: &Connection, username: &str, password: &str) -> Result<Option<Role>, UserError> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT password_hash, role FROM users WHERE username = ?1",
            params![username],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let (stored, role) = match row {
        Some((h, r)) => (h, r.parse::<Role>().ok()),
        None => {
            // Burn comparable time against the dummy hash (anti-enumeration).
            let _ = verify_against(dummy_hash(), password);
            return Ok(None);
        }
    };

    if verify_against(&stored, password) {
        Ok(role)
    } else {
        Ok(None)
    }
}

fn verify_against(phc: &str, password: &str) -> bool {
    PasswordHash::new(phc)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        db::migrate(&mut c).unwrap();
        c
    }

    #[test]
    fn create_and_verify() {
        let c = conn();
        create(&c, "alice", "hunter2!", Role::Admin).unwrap();
        assert_eq!(count(&c).unwrap(), 1);
        assert_eq!(verify(&c, "alice", "hunter2!").unwrap(), Some(Role::Admin));
        assert_eq!(verify(&c, "alice", "wrong").unwrap(), None);
        assert_eq!(verify(&c, "nobody", "hunter2!").unwrap(), None);
    }

    #[test]
    fn duplicate_username_rejected() {
        let c = conn();
        create(&c, "alice", "pw1", Role::Viewer).unwrap();
        assert!(create(&c, "alice", "pw2", Role::Viewer).is_err());
    }

    #[test]
    fn hash_is_argon2id_phc() {
        let h = hash_password("pw").unwrap();
        assert!(h.starts_with("$argon2id$"), "got {h}");
    }
}
