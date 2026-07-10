//! agent.db — the agent's durable local state (rusqlite, WAL).
//!
//! Crash-only (CLAUDE.md Law 3): startup IS recovery — `open` is
//! idempotent, schema creation uses IF NOT EXISTS, every write is
//! one transaction, and `kill -9` at any point leaves a database the
//! next startup resumes from. Offline-first (Law 5): the
//! last-accepted manifest and the applied-state table are what the
//! agent continues from when the network is gone.
//!
//! Tables (docs/decisions/agent.md D5 journal-phase contract;
//! spec/reeve/08-packaging.md §10.2 anti-rollback persistence):
//! - `manifest_state` — single row: last ACCEPTED State Manifest
//!   (version, ETag, body). The monotonicity floor survives restarts.
//! - `journal` — append-only agent event journal (info | notable |
//!   security | error). SECURITY/NOTABLE events required by §10.2
//!   land here (and in stdout logs); REV-004 backfill (B7) will
//!   drain from it later.
//! - `applied_state` — per-app applied phase + content hash
//!   (D5: planned -> applying -> applied | failed; removing ->
//!   removed). B1 creates and reads it ("continue from applied");
//!   the compose provider (B3) drives the phases.
//! - `bundle_state` — single row: digest of the render bundle
//!   currently swapped into place (docs/decisions/tree-render.md D2:
//!   "applied bundle digest recorded in agent.db, not a loose
//!   file"). Written ONLY after the atomic dir swap (B2); startup
//!   recovery rolls it forward from disk if a `kill -9` landed
//!   between swap and record.

use std::path::Path;

use reeve_types::reeve::manifest::{ManifestVersion, StateManifest};
use rusqlite::{Connection, OptionalExtension, params};

/// Errors from the agent state database.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("corrupt stored manifest json: {0}")]
    CorruptManifest(#[from] serde_json::Error),
}

/// Journal entry severity. `security` and `notable` are the exact
/// event classes spec/reeve/08-packaging.md §10.2 requires the agent
/// to log (regression => SECURITY, epoch bump => NOTABLE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Notable,
    Security,
    Error,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Notable => "notable",
            Severity::Security => "security",
            Severity::Error => "error",
        }
    }
}

/// The last-accepted manifest, as persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedManifest {
    pub version: ManifestVersion,
    /// The manifest digest `sha256:<hex>` — sent back as
    /// `If-None-Match` (spec/reeve/08-packaging.md §10.2).
    pub etag: String,
    pub manifest: StateManifest,
}

/// One journal row (read-back shape; used by tests and, later, B7
/// backfill).
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    pub seq: i64,
    pub ts: String,
    pub severity: String,
    pub event: String,
    pub detail: String,
}

/// One applied-state row (docs/decisions/agent.md D5).
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedApp {
    pub app_id: String,
    pub content_hash: String,
    pub secrets_version: Option<String>,
    pub phase: String,
}

/// One unsent status-report row (store-and-forward seed for B7
/// backfill; spec/reeve/05-health-journal.md §7.3).
#[derive(Debug, Clone, PartialEq)]
pub struct StatusRow {
    pub seq: i64,
    /// Original timestamp — assigned when journaled, never rewritten
    /// (§7 "original timestamp"); becomes `reeve.observedAt`.
    pub ts: String,
    pub app_id: String,
    pub deployment_id: String,
    pub body_json: String,
}

/// Handle on agent.db.
pub struct AgentDb {
    conn: Connection,
}

impl AgentDb {
    /// Open (creating if absent) the agent database. Idempotent —
    /// startup IS recovery (Law 3). WAL, foreign_keys ON,
    /// busy_timeout 5s.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            // Creating the data dir is part of idempotent startup.
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS manifest_state (
                id               INTEGER PRIMARY KEY CHECK (id = 1),
                -- ManifestVersion u64 bit-cast to i64. Compared only
                -- in Rust (u64 order != i64 order past bit 63).
                manifest_version INTEGER NOT NULL,
                etag             TEXT NOT NULL,
                manifest_json    TEXT NOT NULL,
                accepted_at      TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS journal (
                seq      INTEGER PRIMARY KEY AUTOINCREMENT,
                ts       TEXT NOT NULL,
                severity TEXT NOT NULL
                         CHECK (severity IN ('info','notable','security','error')),
                event    TEXT NOT NULL,
                detail   TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS applied_state (
                app_id          TEXT PRIMARY KEY,
                content_hash    TEXT NOT NULL,
                secrets_version TEXT,
                phase           TEXT NOT NULL
                                CHECK (phase IN ('planned','applying','applied',
                                                 'failed','removing','removed')),
                updated_at      TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS bundle_state (
                id         INTEGER PRIMARY KEY CHECK (id = 1),
                -- OCI manifest digest of the swapped-in render
                -- bundle, grammar sha256:<hex>.
                digest     TEXT NOT NULL,
                swapped_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS status_reports (
                seq           INTEGER PRIMARY KEY AUTOINCREMENT,
                ts            TEXT NOT NULL,
                app_id        TEXT NOT NULL,
                deployment_id TEXT NOT NULL,
                -- Serialized Margo DeploymentStatusManifest WITHOUT
                -- the reeve extension; the sender attaches
                -- {observedAt: ts, seq} at transmission time
                -- (spec/reeve/05-health-journal.md §7.3).
                body_json     TEXT NOT NULL,
                sent          INTEGER NOT NULL DEFAULT 0 CHECK (sent IN (0, 1))
            );
            "#,
        )?;
        Ok(AgentDb { conn })
    }

    /// The last ACCEPTED manifest — the monotonicity floor
    /// (spec/reeve/08-packaging.md §10.2) and the state the agent
    /// continues from offline (Law 5). `None` before first accept.
    pub fn last_accepted(&self) -> Result<Option<AcceptedManifest>, StateError> {
        let row = self
            .conn
            .query_row(
                "SELECT manifest_version, etag, manifest_json FROM manifest_state WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((v, etag, json)) => Ok(Some(AcceptedManifest {
                version: ManifestVersion(v as u64),
                etag,
                manifest: serde_json::from_str(&json)?,
            })),
        }
    }

    /// Accept a manifest: persist it as the new floor AND journal
    /// the acceptance, atomically (one transaction — kill -9 between
    /// the two must be impossible, Law 3).
    pub fn record_accepted(
        &mut self,
        manifest: &StateManifest,
        etag: &str,
        severity: Severity,
        event: &str,
        detail: &str,
    ) -> Result<(), StateError> {
        let json = serde_json::to_string(manifest)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO manifest_state (id, manifest_version, etag, manifest_json, accepted_at)
             VALUES (1, ?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(id) DO UPDATE SET
                 manifest_version = excluded.manifest_version,
                 etag             = excluded.etag,
                 manifest_json    = excluded.manifest_json,
                 accepted_at      = excluded.accepted_at",
            params![manifest.manifest_version.0 as i64, etag, json],
        )?;
        tx.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?1, ?2, ?3)",
            params![severity.as_str(), event, detail],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Append one journal entry (its own implicit transaction).
    pub fn journal(
        &self,
        severity: Severity,
        event: &str,
        detail: &str,
    ) -> Result<(), StateError> {
        self.conn.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?1, ?2, ?3)",
            params![severity.as_str(), event, detail],
        )?;
        Ok(())
    }

    /// All journal entries in sequence order.
    pub fn journal_entries(&self) -> Result<Vec<JournalEntry>, StateError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, ts, severity, event, detail FROM journal ORDER BY seq")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(JournalEntry {
                    seq: r.get(0)?,
                    ts: r.get(1)?,
                    severity: r.get(2)?,
                    event: r.get(3)?,
                    detail: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The applied-state table — what "continue from last known
    /// state" (Law 5) continues from. B3 writes the phases; B1 only
    /// reads.
    pub fn applied_apps(&self) -> Result<Vec<AppliedApp>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT app_id, content_hash, secrets_version, phase
             FROM applied_state ORDER BY app_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AppliedApp {
                    app_id: r.get(0)?,
                    content_hash: r.get(1)?,
                    secrets_version: r.get(2)?,
                    phase: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Digest (`sha256:<hex>`) of the render bundle currently
    /// swapped into place, if any. `None` before the first pull.
    pub fn pulled_bundle(&self) -> Result<Option<String>, StateError> {
        Ok(self
            .conn
            .query_row(
                "SELECT digest FROM bundle_state WHERE id = 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Record the swapped-in bundle digest AND journal it, atomically
    /// (Law 3: one transaction). Called only AFTER the atomic dir
    /// swap — the swap is the commitment point; this record is the
    /// durable pointer to it (docs/decisions/tree-render.md D2).
    /// `event` is `bundle-swapped` on the pull path and
    /// `bundle-rolled-forward` when startup recovery completes an
    /// interrupted swap-then-record.
    pub fn record_bundle(
        &mut self,
        digest: &str,
        event: &str,
        detail: &str,
    ) -> Result<(), StateError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO bundle_state (id, digest, swapped_at)
             VALUES (1, ?1, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(id) DO UPDATE SET
                 digest     = excluded.digest,
                 swapped_at = excluded.swapped_at",
            params![digest],
        )?;
        tx.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'info', ?1, ?2)",
            params![event, detail],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Clear the bundle record (the on-disk bundle vanished — startup
    /// recovery reconciles the DB to disk truth). NOTABLE: this only
    /// happens on external interference with the data dir.
    pub fn clear_bundle(&mut self, detail: &str) -> Result<(), StateError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM bundle_state WHERE id = 1", [])?;
        tx.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'notable', 'bundle-state-cleared', ?1)",
            params![detail],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record one D5 phase transition: upsert the applied-state row
    /// AND journal the transition, atomically (one transaction,
    /// Law 3 — the phase row and its journal evidence can never
    /// diverge). This is THE call that records intent BEFORE action
    /// (docs/decisions/agent.md D5): converge writes `planned` /
    /// `applying` / `removing` before it acts, and `applied` /
    /// `failed` / `removed` after.
    pub fn record_phase(
        &mut self,
        app_id: &str,
        content_hash: &str,
        secrets_version: Option<&str>,
        phase: &str,
        detail: &str,
    ) -> Result<(), StateError> {
        let severity = if phase == "failed" {
            Severity::Error
        } else {
            Severity::Info
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO applied_state (app_id, content_hash, secrets_version, phase, updated_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(app_id) DO UPDATE SET
                 content_hash    = excluded.content_hash,
                 secrets_version = excluded.secrets_version,
                 phase           = excluded.phase,
                 updated_at      = excluded.updated_at",
            params![app_id, content_hash, secrets_version, phase],
        )?;
        tx.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?1, ?2, ?3)",
            params![severity.as_str(), format!("app-{phase}"), format!("{app_id}: {detail}")],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record one status report locally FIRST (store-and-forward,
    /// spec/reeve/05-health-journal.md §7.3: "journaling MUST NOT
    /// depend on connectivity"). Returns the row's monotonic `seq` —
    /// what the reeve status extension carries. Full backfill
    /// machinery is B7; this table + the sent flag is its seed.
    pub fn record_status(
        &self,
        app_id: &str,
        deployment_id: &str,
        body_json: &str,
    ) -> Result<i64, StateError> {
        self.conn.execute(
            "INSERT INTO status_reports (ts, app_id, deployment_id, body_json, sent)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?1, ?2, ?3, 0)",
            params![app_id, deployment_id, body_json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Unsent status reports in sequence order (§7.3: batches ordered
    /// by sequence number).
    pub fn unsent_statuses(&self) -> Result<Vec<StatusRow>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, app_id, deployment_id, body_json FROM status_reports
             WHERE sent = 0 ORDER BY seq",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(StatusRow {
                    seq: r.get(0)?,
                    ts: r.get(1)?,
                    app_id: r.get(2)?,
                    deployment_id: r.get(3)?,
                    body_json: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark one status report transmitted. Idempotent; resending
    /// after a crash is harmless because the server deduplicates by
    /// `(deviceId, seq)` (spec/reeve/05-health-journal.md §7.3).
    pub fn mark_status_sent(&self, seq: i64) -> Result<(), StateError> {
        self.conn.execute(
            "UPDATE status_reports SET sent = 1 WHERE seq = ?1",
            params![seq],
        )?;
        Ok(())
    }

    /// Upsert one applied-state row (exposed now so B3's provider
    /// has its contract; used by tests).
    pub fn record_applied(
        &self,
        app_id: &str,
        content_hash: &str,
        secrets_version: Option<&str>,
        phase: &str,
    ) -> Result<(), StateError> {
        self.conn.execute(
            "INSERT INTO applied_state (app_id, content_hash, secrets_version, phase, updated_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(app_id) DO UPDATE SET
                 content_hash    = excluded.content_hash,
                 secrets_version = excluded.secrets_version,
                 phase           = excluded.phase,
                 updated_at      = excluded.updated_at",
            params![app_id, content_hash, secrets_version, phase],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_types::reeve::manifest::{BundleRef, StateManifest};

    fn manifest(version: u64) -> StateManifest {
        StateManifest {
            manifest_version: ManifestVersion(version),
            bundle: Some(BundleRef {
                media_type: None,
                digest: format!("sha256:{}", "a".repeat(64)),
                size_bytes: Some(10),
                url: "/v2/x/blobs/sha256:...".into(),
            }),
            apps: vec![],
        }
    }

    #[test]
    fn open_is_idempotent_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        {
            let mut db = AgentDb::open(&path).unwrap();
            db.record_accepted(&manifest(7), "sha256:etag", Severity::Info, "accepted", "")
                .unwrap();
        } // dropped without any shutdown ceremony (Law 3)
        let db = AgentDb::open(&path).unwrap(); // startup IS recovery
        let got = db.last_accepted().unwrap().unwrap();
        assert_eq!(got.version, ManifestVersion(7));
        assert_eq!(got.etag, "sha256:etag");
        assert_eq!(db.journal_entries().unwrap().len(), 1);
        // Re-open once more: schema creation must be idempotent.
        drop(db);
        AgentDb::open(&path).unwrap();
    }

    #[test]
    fn last_accepted_none_before_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        assert!(db.last_accepted().unwrap().is_none());
    }

    #[test]
    fn record_accepted_overwrites_single_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.record_accepted(&manifest(1), "sha256:e1", Severity::Info, "accepted", "")
            .unwrap();
        db.record_accepted(&manifest(2), "sha256:e2", Severity::Info, "accepted", "")
            .unwrap();
        let got = db.last_accepted().unwrap().unwrap();
        assert_eq!(got.version, ManifestVersion(2));
        assert_eq!(got.etag, "sha256:e2");
        assert_eq!(db.journal_entries().unwrap().len(), 2);
    }

    #[test]
    fn manifest_version_roundtrips_past_bit_63() {
        // epoch 0x8000+ sets the sign bit of the i64 storage cast;
        // the bit-cast roundtrip must still be exact.
        let dir = tempfile::tempdir().unwrap();
        let mut db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        let v = ManifestVersion::pack(0xFFFF, 5).unwrap();
        db.record_accepted(&manifest(v.0), "sha256:e", Severity::Info, "accepted", "")
            .unwrap();
        assert_eq!(db.last_accepted().unwrap().unwrap().version, v);
    }

    #[test]
    fn journal_severities_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.journal(Severity::Security, "manifest-regression", "42 -> 41")
            .unwrap();
        db.journal(Severity::Notable, "epoch-bump", "0 -> 1").unwrap();
        let entries = db.journal_entries().unwrap();
        assert_eq!(entries[0].severity, "security");
        assert_eq!(entries[1].severity, "notable");
        assert!(entries[0].seq < entries[1].seq);
    }

    #[test]
    fn bundle_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        assert_eq!(db.pulled_bundle().unwrap(), None);
        let d1 = format!("sha256:{}", "1".repeat(64));
        let d2 = format!("sha256:{}", "2".repeat(64));
        db.record_bundle(&d1, "bundle-swapped", "first").unwrap();
        assert_eq!(db.pulled_bundle().unwrap().as_deref(), Some(d1.as_str()));
        db.record_bundle(&d2, "bundle-swapped", "second").unwrap();
        assert_eq!(db.pulled_bundle().unwrap().as_deref(), Some(d2.as_str()));
        db.clear_bundle("bundle dir vanished").unwrap();
        assert_eq!(db.pulled_bundle().unwrap(), None);
        let events: Vec<String> = db
            .journal_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(
            events,
            vec!["bundle-swapped", "bundle-swapped", "bundle-state-cleared"]
        );
    }

    #[test]
    fn record_phase_upserts_and_journals_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.record_phase("web", "sha256:h1", None, "planned", "hash sha256:h1")
            .unwrap();
        db.record_phase("web", "sha256:h1", None, "applying", "")
            .unwrap();
        db.record_phase("web", "sha256:h1", Some("sv1"), "applied", "")
            .unwrap();
        let apps = db.applied_apps().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].phase, "applied");
        assert_eq!(apps[0].secrets_version.as_deref(), Some("sv1"));
        let events: Vec<String> = db
            .journal_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(events, vec!["app-planned", "app-applying", "app-applied"]);
        // failed phase journals at error severity
        db.record_phase("web", "sha256:h1", None, "failed", "boom")
            .unwrap();
        let last = db.journal_entries().unwrap().pop().unwrap();
        assert_eq!(last.severity, "error");
        assert_eq!(last.event, "app-failed");
        // invalid phase rejected by CHECK, and the journal side of
        // the transaction must not land either (atomicity).
        let before = db.journal_entries().unwrap().len();
        assert!(db.record_phase("web", "sha256:h1", None, "exploded", "").is_err());
        assert_eq!(db.journal_entries().unwrap().len(), before);
    }

    #[test]
    fn status_reports_store_and_forward() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        let s1 = db.record_status("web", "dep-1", "{\"a\":1}").unwrap();
        let s2 = db.record_status("db", "dep-2", "{\"b\":2}").unwrap();
        assert!(s1 < s2, "seq must be monotonic");
        let unsent = db.unsent_statuses().unwrap();
        assert_eq!(unsent.len(), 2);
        assert_eq!(unsent[0].seq, s1);
        assert_eq!(unsent[0].app_id, "web");
        assert!(!unsent[0].ts.is_empty());
        db.mark_status_sent(s1).unwrap();
        let unsent = db.unsent_statuses().unwrap();
        assert_eq!(unsent.len(), 1);
        assert_eq!(unsent[0].seq, s2);
        // marking twice is harmless (crash-resend idempotency)
        db.mark_status_sent(s1).unwrap();
    }

    #[test]
    fn applied_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.record_applied("app-a", "sha256:h1", None, "applied").unwrap();
        db.record_applied("app-a", "sha256:h2", Some("sv1"), "applying")
            .unwrap();
        let apps = db.applied_apps().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].content_hash, "sha256:h2");
        assert_eq!(apps[0].phase, "applying");
        // invalid phase rejected by CHECK
        assert!(db.record_applied("app-b", "sha256:h", None, "exploded").is_err());
    }
}
