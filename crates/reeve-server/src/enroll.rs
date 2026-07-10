//! Enrollment service (docs/decisions/agent.md D4) — the persistence
//! behind device-api's `POST /api/reeve/v1/enroll`.
//!
//! The D4 ceremony: validate the join token, create (or resume) the
//! device row, seed the device's layer in the desired-state tree
//! (tree-render.md D11: `layers/30-device.<device_id>/`), and issue the
//! ONE device credential (auth.md D1).
//!
//! Atomicity (Law 3): all server-table writes — token validation +
//! use-count, device row, token revocation + issuance — happen in ONE
//! IMMEDIATE SQLite transaction. The revision-store commit is sequenced
//! AFTER that transaction on the store's own connection to the same DB
//! file (the store owns its connection; Law 2 forbids reaching into its
//! tables). Crash between the two leaves a fully-enrolled device whose
//! layer dir is absent — semantically identical to an empty layer (D3:
//! absence = inherit), and repaired by the idempotent re-run path the
//! agent's retry takes (it never received the response). Rendering
//! treats a missing device dir as an empty device layer, so no state is
//! ever torn.

use std::sync::{Arc, Mutex};

use device_api::{EnrollError, EnrollRequest, EnrollResponse, EnrollmentService, token_hash};
use revision_store::{RevisionStore, Stream};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

use crate::db::now_secs;
use crate::device_tokens;

/// Generate a fresh device id: `dev-` + 16 lowercase hex chars (64 bits
/// from the OS CSPRNG). Uniqueness is enforced by the devices PRIMARY
/// KEY; a collision at 64 bits fails the insert loudly rather than
/// silently merging identities.
pub fn generate_device_id() -> String {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    format!("dev-{}", hex::encode(buf))
}

/// The device's layer path in the overlay tree (tree-render.md D11).
pub fn device_layer_keep_path(device_id: &str) -> String {
    format!("layers/30-device.{device_id}/.keep")
}

/// [`EnrollmentService`] over the server DB + revision store.
#[derive(Clone)]
pub struct SqliteEnrollmentService {
    db: Arc<Mutex<Connection>>,
    revisions: Arc<Mutex<RevisionStore>>,
}

impl SqliteEnrollmentService {
    pub fn new(db: Arc<Mutex<Connection>>, revisions: Arc<Mutex<RevisionStore>>) -> Self {
        Self { db, revisions }
    }
}

struct JoinTokenRow {
    expires_at: i64,
    max_uses: i64,
    uses: i64,
    device_id: Option<String>,
    revoked_at: Option<i64>,
}

impl EnrollmentService for SqliteEnrollmentService {
    fn enroll(&self, req: &EnrollRequest) -> Result<EnrollResponse, EnrollError> {
        let internal = |e: &dyn std::fmt::Display| EnrollError::Internal(e.to_string());
        let jt_hash = token_hash(&req.join_token);
        let now = now_secs();

        let (device_id, device_token, resumed) = {
            let mut conn = self.db.lock().expect("db mutex poisoned");
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| internal(&e))?;

            let row: Option<JoinTokenRow> = tx
                .query_row(
                    "SELECT expires_at, max_uses, uses, device_id, revoked_at
                     FROM join_tokens WHERE token_hash = ?1",
                    params![jt_hash],
                    |r| {
                        Ok(JoinTokenRow {
                            expires_at: r.get(0)?,
                            max_uses: r.get(1)?,
                            uses: r.get(2)?,
                            device_id: r.get(3)?,
                            revoked_at: r.get(4)?,
                        })
                    },
                )
                .optional()
                .map_err(|e| internal(&e))?;

            let Some(token) = row else {
                return Err(EnrollError::InvalidToken);
            };
            if token.revoked_at.is_some() || now >= token.expires_at {
                return Err(EnrollError::InvalidToken);
            }

            // Idempotent re-run (D4): same join token before expiry +
            // same hostname returns the SAME device, no duplicate rows,
            // and does NOT consume another use (the use was consumed by
            // the run whose response was lost).
            let existing: Option<String> = tx
                .query_row(
                    "SELECT device_id FROM devices
                     WHERE enrolled_with = ?1 AND hostname = ?2",
                    params![jt_hash, req.hostname],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| internal(&e))?;

            let (device_id, resumed) = match existing {
                Some(id) => {
                    tx.execute(
                        "UPDATE devices SET arch = ?2, agent_version = ?3,
                                last_seen_at = ?4, stale = 0
                         WHERE device_id = ?1",
                        params![id, req.arch, req.agent_version, now],
                    )
                    .map_err(|e| internal(&e))?;
                    (id, true)
                }
                None => {
                    if token.uses >= token.max_uses {
                        return Err(EnrollError::InvalidToken);
                    }
                    tx.execute(
                        "UPDATE join_tokens SET uses = uses + 1 WHERE token_hash = ?1",
                        params![jt_hash],
                    )
                    .map_err(|e| internal(&e))?;

                    match &token.device_id {
                        // Re-enroll token (D4): a fresh box resumes the
                        // existing identity and desired state.
                        Some(bound) => {
                            let n = tx
                                .execute(
                                    "UPDATE devices SET hostname = ?2, arch = ?3,
                                            agent_version = ?4, enrolled_with = ?5,
                                            last_seen_at = ?6, stale = 0
                                     WHERE device_id = ?1",
                                    params![
                                        bound,
                                        req.hostname,
                                        req.arch,
                                        req.agent_version,
                                        jt_hash,
                                        now
                                    ],
                                )
                                .map_err(|e| internal(&e))?;
                            if n == 0 {
                                // FK should make this unreachable; fail
                                // closed as an invalid token.
                                return Err(EnrollError::InvalidToken);
                            }
                            (bound.clone(), true)
                        }
                        // Plain join token: a NEW device. Any existing
                        // device with the same hostname is a wiped box's
                        // old identity — flag it stale (D4).
                        None => {
                            let id = generate_device_id();
                            tx.execute(
                                "UPDATE devices SET stale = 1 WHERE hostname = ?1",
                                params![req.hostname],
                            )
                            .map_err(|e| internal(&e))?;
                            tx.execute(
                                "INSERT INTO devices
                                     (device_id, hostname, arch, agent_version,
                                      enrolled_at, labels, enrolled_with, last_seen_at)
                                 VALUES (?1, ?2, ?3, ?4, ?5, '{}', ?6, ?7)",
                                params![
                                    id,
                                    req.hostname,
                                    req.arch,
                                    req.agent_version,
                                    now,
                                    jt_hash,
                                    now
                                ],
                            )
                            .map_err(|e| internal(&e))?;
                            (id, false)
                        }
                    }
                }
            };

            // ONE credential per device (D1): revoke anything prior —
            // the enrolling installer holds the new token; anything else
            // holding an old one is a wiped box or a lost response.
            device_tokens::revoke_all(&tx, &device_id).map_err(|e| internal(&e))?;
            let device_token = device_tokens::issue(&tx, &device_id).map_err(|e| internal(&e))?;

            tx.commit().map_err(|e| internal(&e))?;
            (device_id, device_token, resumed)
        };

        // Initial desired state: seed the device layer in the local
        // stream (D4 step 2, D11). Idempotent — a no-op when the layer
        // already exists (re-enroll/retry), and the store's commit is
        // itself a no-op against an identical head.
        {
            let mut store = self.revisions.lock().expect("revisions mutex poisoned");
            ensure_device_layer(&mut store, &device_id).map_err(|e| internal(&e))?;
        }

        Ok(EnrollResponse {
            device_id,
            device_token,
            resumed,
        })
    }
}

/// Ensure `layers/30-device.<device_id>/` exists in the local stream
/// (tree-render.md D11) by committing an empty `.keep` file. Idempotent:
/// present at head => no new revision.
pub fn ensure_device_layer(
    store: &mut RevisionStore,
    device_id: &str,
) -> Result<(), revision_store::Error> {
    let keep = device_layer_keep_path(device_id);
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(head) = store.head(Stream::Local)? {
        let tree = store.tree_at(head)?;
        if tree.contains_key(&keep) {
            return Ok(());
        }
        // Commits are whole-tree snapshots: carry the head tree forward.
        for (path, digest) in tree {
            let content = store.blob(&digest)?.ok_or_else(|| {
                revision_store::Error::Corrupt(format!("missing blob {digest} for {path}"))
            })?;
            files.push((path, content));
        }
    }
    files.push((keep, Vec::new()));
    store.commit(
        files,
        "system:enroll",
        &format!("enroll device {device_id}"),
        Stream::Local,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::join_tokens;
    use device_api::DeviceTokenStore as _;
    use device_api::device_token::token_hash as dt_hash;

    struct Harness {
        _dir: tempfile::TempDir,
        svc: SqliteEnrollmentService,
        db: Arc<Mutex<Connection>>,
        revisions: Arc<Mutex<RevisionStore>>,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reeve.db");
        let mut conn = crate::db::open(&path).unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let db = Arc::new(Mutex::new(conn));
        let revisions = Arc::new(Mutex::new(RevisionStore::open(&path).unwrap()));
        Harness {
            _dir: dir,
            svc: SqliteEnrollmentService::new(db.clone(), revisions.clone()),
            db,
            revisions,
        }
    }

    fn req(token: &str, hostname: &str) -> EnrollRequest {
        EnrollRequest {
            join_token: token.into(),
            hostname: hostname.into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
        }
    }

    fn plain_token(h: &Harness, ttl: i64, max_uses: i64) -> String {
        join_tokens::issue(&h.db.lock().unwrap(), "op", ttl, max_uses, None).unwrap()
    }

    fn device_count(h: &Harness) -> i64 {
        h.db.lock()
            .unwrap()
            .query_row("SELECT count(*) FROM devices", [], |r| r.get(0))
            .unwrap()
    }

    fn token_authenticates(h: &Harness, raw: &str) -> Option<String> {
        let store = crate::device_tokens::SqliteDeviceTokenStore::new(h.db.clone());
        store.device_id_for_hash(&dt_hash(raw)).unwrap()
    }

    #[test]
    fn happy_path_enrolls_and_seeds_layer() {
        let h = harness();
        let jt = plain_token(&h, 3600, 1);
        let resp = h.svc.enroll(&req(&jt, "edge-01")).unwrap();

        assert!(resp.device_id.starts_with("dev-"));
        assert!(resp.device_token.starts_with("rvd_"));
        assert!(!resp.resumed);

        // device row
        let (hostname, arch, stale): (String, String, i64) = h
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT hostname, arch, stale FROM devices WHERE device_id = ?1",
                params![resp.device_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(hostname, "edge-01");
        assert_eq!(arch, "x86_64");
        assert_eq!(stale, 0);

        // credential authenticates (D1: the ONE token)
        assert_eq!(
            token_authenticates(&h, &resp.device_token),
            Some(resp.device_id.clone())
        );

        // use consumed
        let uses: i64 = h
            .db
            .lock()
            .unwrap()
            .query_row("SELECT uses FROM join_tokens", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uses, 1);

        // initial desired state: device layer in the local stream (D11)
        let store = h.revisions.lock().unwrap();
        let head = store.head(Stream::Local).unwrap().unwrap();
        let tree = store.tree_at(head).unwrap();
        assert!(tree.contains_key(&device_layer_keep_path(&resp.device_id)));
    }

    #[test]
    fn wrong_token_is_rejected() {
        let h = harness();
        let _jt = plain_token(&h, 3600, 1);
        let err = h.svc.enroll(&req("rvj_wrong", "edge-01")).unwrap_err();
        assert!(matches!(err, EnrollError::InvalidToken));
        assert_eq!(device_count(&h), 0);
    }

    #[test]
    fn expired_token_is_rejected() {
        let h = harness();
        let jt = plain_token(&h, -1, 1); // already expired
        let err = h.svc.enroll(&req(&jt, "edge-01")).unwrap_err();
        assert!(matches!(err, EnrollError::InvalidToken));
        assert_eq!(device_count(&h), 0);
    }

    #[test]
    fn revoked_token_is_rejected() {
        let h = harness();
        let jt = plain_token(&h, 3600, 1);
        join_tokens::revoke(&h.db.lock().unwrap(), &token_hash(&jt)).unwrap();
        let err = h.svc.enroll(&req(&jt, "edge-01")).unwrap_err();
        assert!(matches!(err, EnrollError::InvalidToken));
    }

    #[test]
    fn exhausted_token_is_rejected_for_a_different_host() {
        let h = harness();
        let jt = plain_token(&h, 3600, 1);
        h.svc.enroll(&req(&jt, "edge-01")).unwrap();
        // different hostname => not the idempotent path => exhausted
        let err = h.svc.enroll(&req(&jt, "edge-02")).unwrap_err();
        assert!(matches!(err, EnrollError::InvalidToken));
        assert_eq!(device_count(&h), 1);
    }

    #[test]
    fn idempotent_rerun_same_hostname_returns_same_device() {
        let h = harness();
        let jt = plain_token(&h, 3600, 1);
        let first = h.svc.enroll(&req(&jt, "edge-01")).unwrap();
        let second = h.svc.enroll(&req(&jt, "edge-01")).unwrap();

        assert_eq!(second.device_id, first.device_id, "same device (D4)");
        assert!(second.resumed);
        assert_eq!(device_count(&h), 1, "no duplicate rows");
        assert_ne!(second.device_token, first.device_token, "fresh token");
        // prior token revoked: one credential per device (D1)
        assert_eq!(token_authenticates(&h, &first.device_token), None);
        assert_eq!(
            token_authenticates(&h, &second.device_token),
            Some(first.device_id.clone())
        );
        // no extra use consumed
        let uses: i64 = h
            .db
            .lock()
            .unwrap()
            .query_row("SELECT uses FROM join_tokens", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uses, 1);
        // no duplicate revisions: the layer commit was a no-op
        let store = h.revisions.lock().unwrap();
        let head = store.head(Stream::Local).unwrap().unwrap();
        assert!(
            store
                .tree_at(head)
                .unwrap()
                .contains_key(&device_layer_keep_path(&first.device_id))
        );
    }

    #[test]
    fn reenroll_token_resumes_identity_and_desired_state() {
        let h = harness();
        let jt = plain_token(&h, 3600, 1);
        let orig = h.svc.enroll(&req(&jt, "edge-01")).unwrap();

        // operator issues a re-enroll token bound to the device (D4)
        let rt =
            join_tokens::issue(&h.db.lock().unwrap(), "op", 3600, 1, Some(&orig.device_id))
                .unwrap();
        // fresh box (same hostname after a wipe)
        let resumed = h.svc.enroll(&req(&rt, "edge-01")).unwrap();

        assert_eq!(resumed.device_id, orig.device_id, "identity resumed");
        assert!(resumed.resumed);
        assert_eq!(device_count(&h), 1);
        // old credential dead, new one live
        assert_eq!(token_authenticates(&h, &orig.device_token), None);
        assert_eq!(
            token_authenticates(&h, &resumed.device_token),
            Some(orig.device_id.clone())
        );
        // stale cleared
        let stale: i64 = h
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT stale FROM devices WHERE device_id = ?1",
                params![orig.device_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0);
        // desired state (the device layer) still present — resumed, not recreated
        let store = h.revisions.lock().unwrap();
        let head = store.head(Stream::Local).unwrap().unwrap();
        assert!(
            store
                .tree_at(head)
                .unwrap()
                .contains_key(&device_layer_keep_path(&orig.device_id))
        );
    }

    #[test]
    fn plain_token_on_wiped_box_creates_new_device_and_flags_old_stale() {
        let h = harness();
        let jt1 = plain_token(&h, 3600, 1);
        let old = h.svc.enroll(&req(&jt1, "edge-01")).unwrap();

        // wiped box, NEW plain token, same hostname (D4)
        let jt2 = plain_token(&h, 3600, 1);
        let new = h.svc.enroll(&req(&jt2, "edge-01")).unwrap();

        assert_ne!(new.device_id, old.device_id, "new identity");
        assert!(!new.resumed);
        assert_eq!(device_count(&h), 2);
        let stale: i64 = h
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT stale FROM devices WHERE device_id = ?1",
                params![old.device_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 1, "old identity flagged stale (D4)");
    }

    #[test]
    fn ensure_device_layer_preserves_existing_tree() {
        let h = harness();
        {
            let mut store = h.revisions.lock().unwrap();
            store
                .commit(
                    [("layers/00-fleet/apps/nginx/app.yaml", b"enabled: true\n".as_slice())],
                    "op",
                    "fleet",
                    Stream::Local,
                )
                .unwrap();
        }
        let jt = plain_token(&h, 3600, 1);
        let resp = h.svc.enroll(&req(&jt, "edge-01")).unwrap();

        let store = h.revisions.lock().unwrap();
        let head = store.head(Stream::Local).unwrap().unwrap();
        let tree = store.tree_at(head).unwrap();
        assert!(tree.contains_key("layers/00-fleet/apps/nginx/app.yaml"));
        assert!(tree.contains_key(&device_layer_keep_path(&resp.device_id)));
    }

    #[test]
    fn device_id_shape() {
        let id = generate_device_id();
        assert!(id.starts_with("dev-"));
        assert_eq!(id.len(), 4 + 16);
        assert_ne!(id, generate_device_id());
    }
}
