//! SQLite-backed session cookies with sliding expiry (docs/decisions/
//! auth.md D1). The cookie carries a random 256-bit token; the DB stores
//! only its hex sha256 (same rationale as device tokens — high-entropy
//! random value, no stretching needed).

use device_api::{Role, token_hash};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::db::now_secs;

pub const SESSION_COOKIE: &str = "reeve_session";
const SESSION_TOKEN_PREFIX: &str = "rvh_";

/// Sliding-expiry writes are skipped when they'd advance the deadline by
/// less than this — keeps hot sessions from writing on every request.
const SLIDE_GRANULARITY_SECS: i64 = 60;

pub fn generate_session_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    format!("{SESSION_TOKEN_PREFIX}{}", hex::encode(buf))
}

/// Create a session for `username`; returns the raw token for the cookie.
pub fn create(conn: &Connection, username: &str, ttl_secs: i64) -> rusqlite::Result<String> {
    let token = generate_session_token();
    let now = now_secs();
    conn.execute(
        "INSERT INTO sessions (token_hash, username, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![token_hash(&token), username, now, now + ttl_secs],
    )?;
    Ok(token)
}

/// Validate a raw session token; on success returns `(username, role)` and
/// slides the expiry forward (D1 sliding expiry). Expired/unknown => None.
pub fn validate_and_slide(
    conn: &Connection,
    raw_token: &str,
    ttl_secs: i64,
) -> rusqlite::Result<Option<(String, Role)>> {
    let now = now_secs();
    let hash = token_hash(raw_token);
    let row: Option<(String, i64, String)> = conn
        .query_row(
            "SELECT s.username, s.expires_at, u.role
             FROM sessions s JOIN users u ON u.username = s.username
             WHERE s.token_hash = ?1 AND s.expires_at > ?2",
            params![hash, now],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let Some((username, expires_at, role)) = row else {
        return Ok(None);
    };
    let Ok(role) = role.parse::<Role>() else {
        return Ok(None); // CHECK constraint makes this unreachable
    };

    let new_expiry = now + ttl_secs;
    if new_expiry > expires_at + SLIDE_GRANULARITY_SECS {
        conn.execute(
            "UPDATE sessions SET expires_at = ?1 WHERE token_hash = ?2",
            params![new_expiry, hash],
        )?;
    }
    Ok(Some((username, role)))
}

/// Delete the session for a raw token (logout). Idempotent.
pub fn delete(conn: &Connection, raw_token: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM sessions WHERE token_hash = ?1",
        params![token_hash(raw_token)],
    )?;
    Ok(())
}

/// Drop expired sessions. Run at startup (Law 3: startup is recovery —
/// no background reaper needed at this scale). Idempotent.
pub fn purge_expired(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM sessions WHERE expires_at <= ?1",
        params![now_secs()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::users;
    use crate::db;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        db::migrate(&mut c).unwrap();
        users::create(&c, "alice", "pw", Role::Operator).unwrap();
        c
    }

    #[test]
    fn create_validate_delete_round_trip() {
        let c = conn();
        let t = create(&c, "alice", 3600).unwrap();
        assert!(t.starts_with(SESSION_TOKEN_PREFIX));
        assert_eq!(
            validate_and_slide(&c, &t, 3600).unwrap(),
            Some(("alice".to_string(), Role::Operator))
        );
        assert_eq!(validate_and_slide(&c, "rvh_bogus", 3600).unwrap(), None);
        delete(&c, &t).unwrap();
        assert_eq!(validate_and_slide(&c, &t, 3600).unwrap(), None);
    }

    #[test]
    fn expired_session_rejected_and_purged() {
        let c = conn();
        let t = create(&c, "alice", 3600).unwrap();
        c.execute(
            "UPDATE sessions SET expires_at = ?1",
            params![now_secs() - 1],
        )
        .unwrap();
        assert_eq!(validate_and_slide(&c, &t, 3600).unwrap(), None);
        assert_eq!(purge_expired(&c).unwrap(), 1);
    }

    #[test]
    fn expiry_slides_forward_on_use() {
        let c = conn();
        let t = create(&c, "alice", 3600).unwrap();
        // Simulate an old session close to expiry.
        c.execute(
            "UPDATE sessions SET expires_at = ?1",
            params![now_secs() + 10],
        )
        .unwrap();
        validate_and_slide(&c, &t, 3600).unwrap().unwrap();
        let exp: i64 = c
            .query_row("SELECT expires_at FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert!(
            exp >= now_secs() + 3599,
            "expiry must slide forward, got {exp}"
        );
    }
}
