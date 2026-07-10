//! ext-health — Device Health & Status Journal, agent side (B7,
//! REV-004; spec/reeve/05-health-journal.md §7).
//!
//! Two halves:
//! - **Sampler** (§7.2): a background task samples local telemetry
//!   (memory, load, per-filesystem disk, uptime, per-workload restart
//!   counts from the active Provider, agent version, clock skew) on a
//!   config interval, journals every sample locally FIRST (§7.1:
//!   "journaling MUST NOT depend on connectivity"), publishes the
//!   latest sample into the live-status slot
//!   ([`crate::report::StatusSink::health_slot`], §7.3 live path),
//!   and runs the bounded-retention sweep (§7.1).
//! - **Backfill** (§7.3 backfill path): drain unacknowledged journal
//!   records — with their ORIGINAL timestamps — to
//!   `POST /api/reeve/v1/journal/{deviceId}` in seq-ordered batches;
//!   `JournalAck.ackedSeq` advances the persistent watermark that
//!   permits eviction. Offline is accumulation, not loss (Law 5);
//!   a crash between POST and watermark write just resends — the
//!   server dedupes by `(deviceId, seq)` (§7.3, Law 3).
//!
//! The sampler holds its OWN SQLite connection: a sampler failure or
//! stall can never affect convergence (Law 5; §3.2 degradation —
//! core never depends on anything in this module).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reeve_types::reeve::health::{
    DiskSample, HealthSample, JournalAck, JournalBatch, JournalRecord, JournalRecordKind,
    MemorySample,
};
use tracing::{info, warn};

use crate::config::AgentConfig;
use crate::provider::Provider;
use crate::report::SharedHealth;
use crate::state::{AgentDb, WireRecord};

/// Records per backfill batch. The spec pins ordering and idempotency
/// (§7.3), not a size; 256 keeps a batch of worst-case status bodies
/// comfortably under typical request-size limits while draining a
/// month of 60 s samples in ~170 requests.
pub const DEFAULT_BATCH_SIZE: u32 = 256;

/// Latest measured clock skew vs the server, milliseconds (§7.2:
/// "measured opportunistically when connected"). Writer: the backfill
/// sender (from response `Date` headers); reader: the sampler.
pub type SkewSlot = Arc<Mutex<Option<i64>>>;

// ---------------------------------------------------------------
// Sampling (§7.2)
// ---------------------------------------------------------------

/// Collect one point-in-time health sample (§7.2) from /proc and
/// statvfs. Every field is best-effort and independently optional:
/// an unreadable source drops its field, never the sample.
pub fn collect_sample(
    data_dir: &Path,
    restarts: Option<BTreeMap<String, u64>>,
    clock_skew_ms: Option<i64>,
) -> HealthSample {
    let mut disk = BTreeMap::new();
    if let Some(s) = disk_sample(Path::new("/")) {
        disk.insert("/".to_string(), s);
    }
    // The agent's own state filesystem is always "relevant" (§7.2):
    // journal growth and bundle pulls live or die by it.
    if let Some(s) = disk_sample(data_dir) {
        disk.insert(data_dir.display().to_string(), s);
    }
    let mut extra = BTreeMap::new();
    if let Some(uptime) = read_uptime_secs("/proc/uptime") {
        // Extensible sample field (§7.2: receivers MUST ignore
        // unknown fields — and preserve them via `extra`).
        extra.insert("uptimeSecs".to_string(), serde_json::json!(uptime));
    }
    HealthSample {
        disk: (!disk.is_empty()).then_some(disk),
        memory: read_meminfo("/proc/meminfo"),
        load: read_loadavg("/proc/loadavg"),
        restarts,
        agent_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        clock_skew_ms,
        extra,
    }
}

/// Memory usage from /proc/meminfo (`MemTotal` / `MemAvailable`,
/// kB). "Used" is total minus available — the kernel's own idea of
/// reclaimable, not free-minus-buffers arithmetic.
fn read_meminfo(path: &str) -> Option<MemorySample> {
    let text = std::fs::read_to_string(path).ok()?;
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|kb| kb * 1024)
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:")?;
    Some(MemorySample {
        used_bytes: Some(total.saturating_sub(available)),
        total_bytes: Some(total),
        extra: BTreeMap::new(),
    })
}

/// Load averages (1/5/15 min) from /proc/loadavg.
fn read_loadavg(path: &str) -> Option<Vec<f64>> {
    let text = std::fs::read_to_string(path).ok()?;
    let loads: Vec<f64> = text
        .split_whitespace()
        .take(3)
        .filter_map(|v| v.parse().ok())
        .collect();
    (loads.len() == 3).then_some(loads)
}

/// System uptime in seconds from /proc/uptime.
fn read_uptime_secs(path: &str) -> Option<f64> {
    std::fs::read_to_string(path)
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Disk usage/free for the filesystem holding `path`, via statvfs
/// (§7.2 — /proc exposes no per-filesystem usage; `free` is
/// `f_bavail`, what unprivileged writers actually have).
// statvfs field widths vary by target (fsblkcnt_t is u32 on some
// 32-bit/musl targets); the casts are load-bearing off x86_64-glibc.
#[allow(clippy::unnecessary_cast)]
fn disk_sample(path: &Path) -> Option<DiskSample> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut vfs = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: c_path is a valid NUL-terminated path and vfs is a
    // properly sized out-parameter; statvfs fully initializes it on
    // the success path we require below.
    if unsafe { libc::statvfs(c_path.as_ptr(), vfs.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so the struct is initialized.
    let vfs = unsafe { vfs.assume_init() };
    let frsize = if vfs.f_frsize > 0 { vfs.f_frsize } else { vfs.f_bsize } as u64;
    let total = (vfs.f_blocks as u64).saturating_mul(frsize);
    let free = (vfs.f_bavail as u64).saturating_mul(frsize);
    let unused = (vfs.f_bfree as u64).saturating_mul(frsize);
    Some(DiskSample {
        used_bytes: Some(total.saturating_sub(unused)),
        free_bytes: Some(free),
        total_bytes: Some(total),
        extra: BTreeMap::new(),
    })
}

// ---------------------------------------------------------------
// Runtime wiring
// ---------------------------------------------------------------

/// Compiled-in handle the binary shell keeps in its `ExtHooks`:
/// the running sampler task plus the backfill sender (if any).
pub struct HealthRuntime {
    /// `None` for `dir://` sources and unenrolled agents — the
    /// sampler still journals locally (§7.1 local-first); records
    /// accumulate until there is a server to backfill.
    sender: Option<JournalSender>,
    /// Detached sampler task (crash-only: no shutdown ceremony —
    /// the process exits, the task dies, agent.db is consistent).
    _task: tokio::task::JoinHandle<()>,
}

impl HealthRuntime {
    pub fn sender(&self) -> Option<&JournalSender> {
        self.sender.as_ref()
    }
}

/// Start the health extension: spawn the sampler task and build the
/// backfill sender. Called once from the binary shell behind the
/// `ext-health` feature gate; core never calls in here.
pub fn spawn(
    cfg: &AgentConfig,
    provider: Arc<dyn Provider + Send + Sync>,
    live_slot: Option<SharedHealth>,
) -> HealthRuntime {
    let skew: SkewSlot = Arc::new(Mutex::new(None));
    let sender = JournalSender::from_config(
        &cfg.server,
        cfg.device_token.clone(),
        cfg.device_id.clone(),
        skew.clone(),
    );
    if sender.is_none() {
        info!("no journal sender (dir:// source or not enrolled); health journals locally only");
    }
    let sampler = Sampler {
        db_path: cfg.db_path(),
        data_dir: cfg.data_dir.clone(),
        interval: Duration::from_secs(cfg.health_interval_secs.max(1)),
        retention_days: cfg.journal_retention_days,
        max_bytes: cfg.journal_max_bytes,
        provider,
        live_slot,
        skew,
    };
    HealthRuntime {
        sender,
        _task: tokio::spawn(sampler.run()),
    }
}

/// The background sampler (§7.2): sample -> journal locally ->
/// publish to the live slot -> retention sweep -> sleep. Every step
/// is best-effort; nothing here can fail convergence (Law 5).
struct Sampler {
    db_path: PathBuf,
    data_dir: PathBuf,
    interval: Duration,
    retention_days: u32,
    max_bytes: u64,
    provider: Arc<dyn Provider + Send + Sync>,
    live_slot: Option<SharedHealth>,
    skew: SkewSlot,
}

impl Sampler {
    async fn run(self) {
        // Own connection (WAL): the sampler never contends with the
        // converge loop's AgentDb borrow.
        let db = match AgentDb::open(&self.db_path) {
            Ok(db) => db,
            Err(e) => {
                warn!(error = %e, "health sampler cannot open agent.db; sampling disabled");
                return;
            }
        };
        loop {
            let skew = self.skew.lock().ok().and_then(|g| *g);
            let sample = collect_sample(&self.data_dir, self.provider.restart_counts(), skew);
            if let Some(slot) = &self.live_slot
                && let Ok(mut guard) = slot.lock()
            {
                *guard = Some(sample.clone());
            }
            match serde_json::to_string(&sample) {
                Ok(json) => {
                    if let Err(e) = db.record_health(&json) {
                        warn!(error = %e, "could not journal health sample");
                    }
                }
                Err(e) => warn!(error = %e, "could not serialize health sample"),
            }
            match db.evict_journal(self.retention_days, self.max_bytes) {
                Ok(Some(gap)) => warn!(
                    from_seq = gap.from_seq,
                    to_seq = gap.to_seq,
                    records = gap.records,
                    "size bound forced eviction of unacknowledged journal records; gap mark journaled (§7.1)"
                ),
                Ok(None) => {}
                Err(e) => warn!(error = %e, "journal retention sweep failed"),
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

// ---------------------------------------------------------------
// Backfill (§7.3)
// ---------------------------------------------------------------

/// Where backfill batches go: `POST /api/reeve/v1/journal/{deviceId}`
/// on the reeve server, device-token authenticated (§7.3).
pub struct JournalSender {
    base: String,
    device_token: Option<String>,
    device_id: String,
    client: reqwest::Client,
    batch_size: u32,
    skew: SkewSlot,
    /// Cleared when the server answers 404/405 — a vanilla Margo WFM
    /// has no journal surface (§3.2 degradation); we stop knocking
    /// until the next restart's capability re-probe.
    supported: AtomicBool,
}

impl JournalSender {
    /// Construct from agent config values. HTTP(S) + enrolled
    /// (device_id) only — same preconditions as the status sink.
    pub fn from_config(
        server: &str,
        device_token: Option<String>,
        device_id: Option<String>,
        skew: SkewSlot,
    ) -> Option<Self> {
        if !(server.starts_with("https://") || server.starts_with("http://")) {
            return None;
        }
        let device_id = device_id?;
        Some(JournalSender {
            base: server.trim_end_matches('/').to_string(),
            device_token,
            device_id,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("static reqwest client config"),
            batch_size: DEFAULT_BATCH_SIZE,
            skew,
            supported: AtomicBool::new(true),
        })
    }

    /// Override the batch size (tests exercise multi-batch drains
    /// without journaling hundreds of records).
    pub fn with_batch_size(mut self, batch_size: u32) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    fn journal_url(&self) -> String {
        format!("{}/api/reeve/v1/journal/{}", self.base, self.device_id)
    }
}

/// A stored wire row as its wire shape (§7.3): the ORIGINAL
/// timestamp travels; the payload is re-emitted verbatim.
fn wire_to_record(row: WireRecord) -> JournalRecord {
    JournalRecord {
        seq: row.seq.max(0) as u64,
        observed_at: row.ts,
        kind: match row.kind.as_str() {
            "status" => JournalRecordKind::Status,
            "health" => JournalRecordKind::Health,
            "gap" => JournalRecordKind::Gap,
            // "lifecycle" — the CHECK constraint pins the set; an
            // impossible stranger degrades to lifecycle rather than
            // punching a hole in the seq stream.
            _ => JournalRecordKind::Lifecycle,
        },
        payload: row
            .payload
            .map(|p| serde_json::from_str(&p).unwrap_or(serde_json::Value::String(p))),
    }
}

/// Opportunistic clock-skew measurement (§7.2): server `Date` header
/// minus local clock, at response receipt. Second-granularity is
/// plenty — skew matters at the scale of minutes-to-days of drift on
/// fanless boxes, not milliseconds.
fn observe_skew(slot: &SkewSlot, headers: &reqwest::header::HeaderMap) {
    let Some(date) = headers
        .get(reqwest::header::DATE)
        .and_then(|v| v.to_str().ok())
    else {
        return;
    };
    let Ok(server) = httpdate::parse_http_date(date) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let skew_ms = match server.duration_since(now) {
        Ok(ahead) => i64::try_from(ahead.as_millis()).unwrap_or(i64::MAX),
        Err(behind) => -i64::try_from(behind.duration().as_millis()).unwrap_or(i64::MAX),
    };
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(skew_ms);
    }
}

/// Drain unacknowledged journal records to the server (§7.3 backfill
/// path), in seq-ordered batches with original timestamps. Runs
/// every cycle — that IS both "on reconnect" and "periodically as a
/// sweep": while offline it returns immediately on the first send
/// error and everything accumulates (Law 5).
///
/// Watermark discipline: `JournalAck.ackedSeq` (highest contiguously
/// ingested) advances the persistent watermark, monotonically. A
/// crash after POST but before the watermark write resends the batch;
/// the server deduplicates by `(deviceId, seq)` (§7.3, Law 3).
pub async fn backfill(db: &AgentDb, sender: &JournalSender) {
    if !sender.supported.load(Ordering::Relaxed) {
        return;
    }
    loop {
        let watermark = match db.journal_watermark() {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "cannot read journal watermark");
                return;
            }
        };
        let rows = match db.unacked_wire_records(sender.batch_size) {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "cannot read unacknowledged journal records");
                return;
            }
        };
        let Some(last_seq) = rows.last().map(|r| r.seq) else {
            return; // nothing unacknowledged
        };
        let full_batch = rows.len() as u32 == sender.batch_size;
        let batch = JournalBatch {
            records: rows.into_iter().map(wire_to_record).collect(),
        };
        let mut req = sender.client.post(sender.journal_url()).json(&batch);
        if let Some(token) = &sender.device_token {
            req = req.bearer_auth(token);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                // Expected operation, not an error (Law 5).
                info!(reason = %e, "journal endpoint unreachable; backfill accumulates");
                return;
            }
        };
        observe_skew(&sender.skew, resp.headers());
        let status = resp.status();
        if status.as_u16() == 404 || status.as_u16() == 405 {
            info!(
                "server has no journal surface (vanilla Margo, §3.2); backfill disabled until restart"
            );
            sender.supported.store(false, Ordering::Relaxed);
            return;
        }
        if status.is_server_error() {
            warn!(%status, "journal ingest server error; backfill retries next cycle");
            return;
        }
        if !status.is_success() {
            warn!(%status, "journal batch rejected; backfill retries next cycle");
            return;
        }
        let ack: JournalAck = match resp.json().await {
            Ok(ack) => ack,
            Err(e) => {
                warn!(error = %e, "unparseable journal ack; backfill retries next cycle");
                return;
            }
        };
        let acked = i64::try_from(ack.acked_seq).unwrap_or(i64::MAX);
        if acked > watermark
            && let Err(e) = db.set_journal_watermark(acked)
        {
            warn!(error = %e, "sent but could not persist watermark; will resend (server dedupes)");
            return;
        }
        if acked < last_seq {
            // The server's contiguous ack stopped short — a hole
            // upstream (e.g. records lost to a §7.1 forced eviction
            // before they were ever sent). Do not spin on it.
            info!(
                acked,
                last_seq, "journal ack behind transmitted seq; retrying next cycle"
            );
            return;
        }
        if !full_batch {
            return; // drained
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentDb;
    use axum::extract::{Json, Path as AxPath, State};
    use axum::routing::post;
    use std::collections::BTreeSet;

    // ---- sampler --------------------------------------------------

    /// §7.2: the sampler emits a valid, round-trippable HealthSample
    /// with plausible values from the real /proc + statvfs.
    #[test]
    fn sample_has_valid_shape() {
        let restarts = BTreeMap::from([("web".to_string(), 2u64)]);
        let sample = collect_sample(Path::new("/tmp"), Some(restarts), Some(-120));
        let json = serde_json::to_value(&sample).unwrap();

        let total = json["memory"]["totalBytes"].as_u64().unwrap();
        let used = json["memory"]["usedBytes"].as_u64().unwrap();
        assert!(total > 0 && used <= total, "memory sane: {used}/{total}");
        assert_eq!(json["load"].as_array().unwrap().len(), 3, "1/5/15 min");
        let root = &json["disk"]["/"];
        assert!(root["totalBytes"].as_u64().unwrap() > 0);
        assert!(root["freeBytes"].as_u64().unwrap() <= root["totalBytes"].as_u64().unwrap());
        assert_eq!(json["restarts"]["web"], 2);
        assert_eq!(json["agentVersion"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["clockSkewMs"], -120);
        // Extensible field rides flattened (§7.2).
        assert!(json["uptimeSecs"].as_f64().unwrap() > 0.0);

        // Round-trips through the wire type without loss.
        let back: HealthSample = serde_json::from_value(json).unwrap();
        assert_eq!(back, sample);
    }

    /// Missing/unreadable sources drop fields, never the sample.
    #[test]
    fn unreadable_sources_degrade_to_absent_fields() {
        assert!(read_meminfo("/nonexistent/meminfo").is_none());
        assert!(read_loadavg("/nonexistent/loadavg").is_none());
        assert!(read_uptime_secs("/nonexistent/uptime").is_none());
        assert!(disk_sample(Path::new("/nonexistent/mount/point")).is_none());
        let sample = collect_sample(Path::new("/nonexistent"), None, None);
        // Still a sample: version is compiled in even if /proc left.
        assert_eq!(sample.agent_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    // ---- mock journal server (§7.3 semantics) ----------------------

    /// Mirror of the server's ingest semantics: idempotent by seq,
    /// ack = highest contiguously ingested from the lowest held.
    #[derive(Default)]
    struct MockJournal {
        /// Arrival order of NEW records: (seq, observedAt, kind).
        records: Vec<(u64, String, String)>,
        seqs: BTreeSet<u64>,
        requests: usize,
        duplicates: usize,
    }

    impl MockJournal {
        fn ack(&self) -> u64 {
            let mut acked: Option<u64> = None;
            for &seq in &self.seqs {
                match acked {
                    None => acked = Some(seq),
                    Some(a) if seq == a + 1 => acked = Some(seq),
                    Some(_) => break,
                }
            }
            acked.unwrap_or(0)
        }
    }

    type Shared = Arc<Mutex<MockJournal>>;

    async fn ingest(
        State(state): State<Shared>,
        AxPath(_device_id): AxPath<String>,
        Json(batch): Json<JournalBatch>,
    ) -> Json<JournalAck> {
        let mut state = state.lock().unwrap();
        state.requests += 1;
        for record in batch.records {
            if state.seqs.insert(record.seq) {
                let kind = serde_json::to_value(record.kind).unwrap();
                state.records.push((
                    record.seq,
                    record.observed_at,
                    kind.as_str().unwrap().to_string(),
                ));
            } else {
                state.duplicates += 1;
            }
        }
        Json(JournalAck { acked_seq: state.ack() })
    }

    async fn serve(state: Shared) -> String {
        let app = axum::Router::new()
            .route("/api/reeve/v1/journal/{device_id}", post(ingest))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn sender_to(base: &str, batch: u32) -> JournalSender {
        JournalSender::from_config(
            base,
            Some("tok-dev-1".into()),
            Some("dev-1".into()),
            SkewSlot::default(),
        )
        .unwrap()
        .with_batch_size(batch)
    }

    /// Journal three kinds of records while "offline", then drain:
    /// seq order preserved across batches, ORIGINAL timestamps on the
    /// wire, watermark = ack afterwards, second sweep sends nothing.
    #[tokio::test]
    async fn backfill_drains_in_order_with_original_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.journal(crate::state::Severity::Info, "agent-start", "v0").unwrap();
        db.record_status("web", "dep-1", "{\"a\":1}").unwrap();
        db.record_health("{\"load\":[0.1,0.2,0.3]}").unwrap();
        let stored = db.wire_records().unwrap();
        assert_eq!(stored.len(), 3);

        let state = Shared::default();
        let base = serve(state.clone()).await;
        // Batch of 2 forces a multi-request drain.
        let sender = sender_to(&base, 2);
        backfill(&db, &sender).await;

        {
            let state = state.lock().unwrap();
            assert_eq!(state.requests, 2, "3 records / batch of 2");
            assert_eq!(state.duplicates, 0);
            let got: Vec<(u64, &str, &str)> = state
                .records
                .iter()
                .map(|(s, ts, k)| (*s, ts.as_str(), k.as_str()))
                .collect();
            assert_eq!(
                got,
                vec![
                    (1, stored[0].ts.as_str(), "lifecycle"),
                    (2, stored[1].ts.as_str(), "status"),
                    (3, stored[2].ts.as_str(), "health"),
                ],
                "seq order, original timestamps, kinds"
            );
        }
        assert_eq!(db.journal_watermark().unwrap(), 3, "ack advanced the watermark");
        assert!(db.unacked_wire_records(10).unwrap().is_empty());

        // Drained: another sweep must not knock at all.
        backfill(&db, &sender).await;
        assert_eq!(state.lock().unwrap().requests, 2);
    }

    /// Law 3: a crash between POST and the watermark write resends
    /// the batch; the server's `(deviceId, seq)` dedup makes that a
    /// no-op and the ack restores the watermark.
    #[tokio::test]
    async fn crash_resend_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        for i in 0..3 {
            db.record_health(&format!("{{\"i\":{i}}}")).unwrap();
        }
        let state = Shared::default();
        let base = serve(state.clone()).await;
        let sender = sender_to(&base, 16);

        backfill(&db, &sender).await;
        assert_eq!(db.journal_watermark().unwrap(), 3);

        // "kill -9" after the POST landed but before the watermark
        // write: simulated by rolling the watermark back.
        db.set_journal_watermark(0).unwrap();
        backfill(&db, &sender).await;

        let state = state.lock().unwrap();
        assert_eq!(state.seqs.len(), 3, "no record ingested twice");
        assert_eq!(state.duplicates, 3, "resend arrived and was deduplicated");
        drop(state);
        assert_eq!(db.journal_watermark().unwrap(), 3, "ack restored the watermark");
    }

    /// Law 5: offline is accumulation. Nothing lost, nothing marked,
    /// watermark untouched.
    #[tokio::test]
    async fn offline_backfill_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        for i in 0..3 {
            db.record_health(&format!("{{\"i\":{i}}}")).unwrap();
        }
        // Nothing listens on this port.
        let sender = sender_to("http://127.0.0.1:1", 16);
        backfill(&db, &sender).await;
        assert_eq!(db.journal_watermark().unwrap(), 0);
        assert_eq!(db.unacked_wire_records(10).unwrap().len(), 3);
    }

    /// §3.2 degradation: a vanilla Margo server (404 on the reeve
    /// journal surface) disables backfill until restart instead of
    /// knocking every cycle.
    #[tokio::test]
    async fn vanilla_server_disables_backfill_until_restart() {
        use axum::http::StatusCode;
        let hits = Arc::new(Mutex::new(0usize));
        let counter = hits.clone();
        let app = axum::Router::new().fallback(move || {
            *counter.lock().unwrap() += 1;
            async { StatusCode::NOT_FOUND }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.record_health("{}").unwrap();
        let sender = sender_to(&format!("http://{addr}"), 16);
        backfill(&db, &sender).await;
        backfill(&db, &sender).await;
        assert_eq!(*hits.lock().unwrap(), 1, "second sweep never knocked");
        assert_eq!(db.unacked_wire_records(10).unwrap().len(), 1, "record retained");
    }

    /// §7.2 clock skew: measured opportunistically from the response
    /// Date header.
    #[tokio::test]
    async fn backfill_measures_clock_skew_from_date_header() {
        async fn ingest_dated(Json(_): Json<JournalBatch>) -> ([(&'static str, &'static str); 1], Json<JournalAck>) {
            (
                [("date", "Mon, 01 Jan 2035 00:00:00 GMT")],
                Json(JournalAck { acked_seq: 1 }),
            )
        }
        let app = axum::Router::new()
            .route("/api/reeve/v1/journal/{device_id}", post(ingest_dated));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.record_health("{}").unwrap();
        let skew = SkewSlot::default();
        let sender = JournalSender::from_config(
            &format!("http://{addr}"),
            None,
            Some("dev-1".into()),
            skew.clone(),
        )
        .unwrap();
        backfill(&db, &sender).await;
        let measured = skew.lock().unwrap().expect("skew measured");
        const YEAR_MS: i64 = 365 * 24 * 3600 * 1000;
        assert!(measured > YEAR_MS, "2035 is years ahead of now: {measured}");
    }

    /// A gap mark left by forced eviction (§7.1) travels the backfill
    /// path like any record. The server's contiguous ack starts at
    /// the lowest seq it HOLDS, so a never-transmitted evicted prefix
    /// is not a wedge — the gap record is what tells the server the
    /// missing range was evicted rather than lost.
    #[tokio::test]
    async fn gap_mark_is_backfilled_after_forced_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        let filler = format!("{{\"pad\":\"{}\"}}", "x".repeat(256));
        for _ in 0..4 {
            db.record_health(&filler).unwrap(); // seqs 1..4
        }
        let gap = db.evict_journal(30, 900).unwrap().expect("forced eviction");
        assert_eq!((gap.from_seq, gap.to_seq), (1, 2));

        let state = Shared::default();
        let base = serve(state.clone()).await;
        let sender = sender_to(&base, 16);
        backfill(&db, &sender).await;

        let state = state.lock().unwrap();
        // Server holds 3,4,5(gap); 1-2 are the eviction the gap
        // record explains.
        assert_eq!(state.seqs.iter().copied().collect::<Vec<_>>(), vec![3, 4, 5]);
        assert_eq!(state.records.last().unwrap().2, "gap");
        drop(state);
        assert_eq!(db.journal_watermark().unwrap(), 5, "everything transmitted is acked");
        assert!(db.unacked_wire_records(10).unwrap().is_empty());
    }
}
