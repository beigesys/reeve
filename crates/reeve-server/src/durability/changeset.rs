//! Changeset CAPTURE tier — seconds-RPO, in-binary
//! (spec/reeve/07-durability.md §9.3, docs/decisions/storage.md D16).
//! Behind `ext-durability-changeset`; replay/restore stay core.
//!
//! The trunk SQLite session extension (rusqlite `session`) rides THE
//! single writer connection (D6 — writer unification is exactly what
//! session capture requires). Every N seconds or M commits the session
//! is extracted, gzip'd, AEAD-sealed under the D15 keyfile, and
//! uploaded under a strictly sequenced key chained to the current
//! snapshot generation. Empty session => no upload.
//!
//! Crash-only (§9.3): the in-memory session lost to kill -9 costs at
//! most the configured interval — that IS the RPO. Every process start
//! cuts a fresh generation (mod.rs `startup`), so the chain never has
//! to span a session that died with its process.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use rusqlite::Connection;
use rusqlite::session::Session;
use tracing::warn;

use crate::db::now_secs;
use crate::durability::snapshot::SnapshotTier;
use crate::durability::target::Target;
use crate::durability::{BoxFut, Durability, DurabilityStatus, aead, verify};

/// A session pinned to THE writer connection.
///
/// SAFETY of the `'static` lifetime + `Send`: the session's raw
/// pointers reference the heap-allocated `sqlite3*`, whose address is
/// independent of where the `Connection` struct lives; `_pin` keeps
/// that connection open for as long as the session exists. Every
/// creation, extraction, and drop of the session happens while holding
/// the `db` mutex (the same serialization all connection use gets), so
/// the session is never touched concurrently with the connection.
struct CapturedSession {
    // Declared first: dropped (sqlite3session_delete) before `_pin`
    // could ever be the last thing keeping the connection open.
    session: Session<'static>,
    _pin: Arc<Mutex<Connection>>,
}

unsafe impl Send for CapturedSession {}

/// Create a capture session on the writer. `conn` MUST be the guarded
/// connection inside `pin` — the caller holds the lock.
fn attach_session_on(
    conn: &Connection,
    pin: Arc<Mutex<Connection>>,
) -> anyhow::Result<CapturedSession> {
    let session = Session::new(conn).context("creating sqlite session")?;
    // Erase the borrow of the mutex guard; `_pin` carries the real
    // ownership relationship (see CapturedSession SAFETY).
    let mut session: Session<'static> = unsafe { std::mem::transmute(session) };
    session
        .attach(None::<&str>)
        .context("attaching session to all tables")?;
    Ok(CapturedSession { session, _pin: pin })
}

/// An extracted-but-not-yet-uploaded changeset. In memory only —
/// losing it to kill -9 is inside the RPO by definition (§9.3).
struct Pending {
    generation: String,
    seq: u64,
    sealed: Vec<u8>,
}

#[derive(Default)]
struct CsState {
    session: Option<CapturedSession>,
    /// The UPLOADED generation the live session chains to. `None`
    /// until the first successful snapshot of this process — shipping
    /// is paused (never mis-chained) while degraded (§9.2 retry owns
    /// recovery).
    session_generation: Option<String>,
    next_seq: u64,
    pending: VecDeque<Pending>,
    last_ship_unix: i64,
    commits_at_last_ship: u64,
    last_uploaded_seq: Option<u64>,
    last_upload_at: Option<i64>,
}

pub struct ChangesetTier {
    snap: SnapshotTier,
    cs: Arc<Mutex<CsState>>,
    commits: Arc<AtomicU64>,
}

impl ChangesetTier {
    pub fn new(snap: SnapshotTier) -> anyhow::Result<Self> {
        // Commit counter for the "every M commits" trigger (§9.3).
        let commits = Arc::new(AtomicU64::new(0));
        {
            let conn = snap.db.lock().expect("db mutex poisoned");
            let counter = commits.clone();
            conn.commit_hook(Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false // never roll back
            }))
            .context("installing commit hook")?;
        }
        Ok(ChangesetTier {
            snap,
            cs: Arc::new(Mutex::new(CsState::default())),
            commits,
        })
    }

    /// Extract + upload if due. Lock order everywhere in this module:
    /// db BEFORE cs, uploads outside both.
    async fn ship(&self) -> anyhow::Result<()> {
        let now = now_secs();
        {
            let st = self.cs.lock().expect("cs state poisoned");
            let commits = self.commits.load(Ordering::Relaxed);
            let interval_due =
                now - st.last_ship_unix >= self.snap.cfg.changeset_interval_secs as i64;
            let commits_due = commits.saturating_sub(st.commits_at_last_ship)
                >= self.snap.cfg.changeset_commits.max(1);
            if !(interval_due || commits_due) {
                return Ok(());
            }
        }

        // Extract under the writer lock: no commit can interleave
        // between "read the session" and "reset the session", so the
        // chain has neither gaps nor duplicates.
        let mut uploads: Vec<(String, u64, Vec<u8>)> = Vec::new();
        {
            let conn = self.snap.db.lock().expect("db mutex poisoned");
            let mut st = self.cs.lock().expect("cs state poisoned");
            st.last_ship_unix = now;
            st.commits_at_last_ship = self.commits.load(Ordering::Relaxed);

            if let Some(generation) = st.session_generation.clone() {
                let extracted: Option<Vec<u8>> = match st.session.as_mut() {
                    Some(cap) if !cap.session.is_empty() => {
                        let mut buf = Vec::new();
                        cap.session
                            .changeset_strm(&mut buf)
                            .context("extracting changeset")?;
                        Some(buf)
                    }
                    _ => None,
                };
                if let Some(raw) = extracted
                    && !raw.is_empty()
                {
                    // Reset capture BEFORE releasing the writer lock —
                    // extraction returns everything since attach, so a
                    // fresh session is what makes sequences disjoint.
                    match attach_session_on(&conn, self.snap.db.clone()) {
                        Ok(next) => st.session = Some(next),
                        Err(e) => {
                            // Capture is broken: stop chaining rather
                            // than double-ship. Next snapshot re-anchors.
                            st.session = None;
                            st.session_generation = None;
                            warn!(error = %e, "durability: session re-attach failed; \
                                   changeset capture paused until next snapshot");
                        }
                    }
                    let seq = st.next_seq;
                    st.next_seq += 1;
                    let sealed = aead::seal(&self.snap.key, &gzip(&raw)?)?;
                    st.pending.push_back(Pending {
                        generation,
                        seq,
                        sealed,
                    });
                }
            }
            for p in &st.pending {
                uploads.push((p.generation.clone(), p.seq, p.sealed.clone()));
            }
        }

        // Upload strictly in order; on failure keep the tail pending
        // and retry next tick (§9.2 degraded-not-fatal applies).
        for (generation, seq, sealed) in uploads {
            match self
                .snap
                .target
                .put(&Target::changeset_key(&generation, seq), sealed)
                .await
            {
                Ok(()) => {
                    let mut st = self.cs.lock().expect("cs state poisoned");
                    if st
                        .pending
                        .front()
                        .is_some_and(|p| p.seq == seq && p.generation == generation)
                    {
                        st.pending.pop_front();
                    }
                    st.last_uploaded_seq = Some(seq);
                    st.last_upload_at = Some(now_secs());
                }
                Err(e) => {
                    self.snap.mark_degraded(&e);
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

fn gzip(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::io::Write as _;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes)?;
    Ok(enc.finish()?)
}

impl Durability for ChangesetTier {
    fn tier(&self) -> &'static str {
        "changeset"
    }

    /// Cut a generation with the session reset INSIDE the same writer
    /// critical section as `VACUUM INTO` (§9.3: a changeset sequence
    /// chains to exactly one snapshot; no commit can fall between).
    fn snapshot_now(&self) -> BoxFut<'_, anyhow::Result<Option<String>>> {
        Box::pin(async move {
            let cs = self.cs.clone();
            let pin = self.snap.db.clone();
            let result = self
                .snap
                .snapshot_with(move |conn| {
                    let mut st = cs.lock().expect("cs state poisoned");
                    // Everything captured (and everything pending) is in
                    // the snapshot being cut — drop it.
                    st.pending.clear();
                    st.next_seq = 1;
                    st.session_generation = None;
                    st.session = match attach_session_on(conn, pin) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            warn!(error = %e, "durability: session attach failed at \
                                   generation cut; changeset capture paused");
                            None
                        }
                    };
                })
                .await;
            if let Ok(Some(generation)) = &result {
                let mut st = self.cs.lock().expect("cs state poisoned");
                if st.session.is_some() {
                    st.session_generation = Some(generation.clone());
                }
            }
            result
        })
    }

    fn ship_changesets(&self) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(self.ship())
    }

    fn verify_restore(&self) -> BoxFut<'_, anyhow::Result<verify::VerifyOutcome>> {
        Box::pin(verify::run(
            &self.snap.db,
            &self.snap.target,
            &self.snap.key,
            &self.snap.cfg,
            &self.snap.work_dir,
        ))
    }

    fn status(&self) -> DurabilityStatus {
        let mut status = self.snap.base_status();
        status.tier = "changeset".into();
        let st = self.cs.lock().expect("cs state poisoned");
        status.last_changeset_seq = st.last_uploaded_seq;
        status.last_changeset_at = st.last_upload_at;
        status.pending_changesets = st.pending.len();
        status
    }
}
