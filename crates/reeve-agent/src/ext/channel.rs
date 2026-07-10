//! ext-channel (REV-001) — the persistent agent→server channel
//! (build item B5).
//!
//! Normative source: spec/reeve/02-channel.md §4 — THE spec for this
//! module. Load-bearing rules implemented here:
//! - §4.1: ALWAYS agent-initiated, outbound, websocket upgrade of
//!   `GET /api/reeve/v1/channel` carrying the device bearer token.
//!   The agent MUST NOT attempt the channel unless the server
//!   advertises `rev-001/1`; upgrade failure is feature-unavailable —
//!   log once, keep polling, retry on the reconnect schedule.
//! - §4.2: control frames (text JSON, unknown `type` ignored) and
//!   data frames (binary, u32 BE sub-channel id prefix). Sub-channel
//!   ids: agent odd, server even. `open` for an unsupported purpose
//!   => `reject`, never teardown. Data for a non-open id: discarded
//!   silently. Channel teardown closes all sub-channels — a normal
//!   event, not corruption.
//! - §4.3: ping when idle ≥ 30 s; missing pong within 10 s = dead
//!   channel (close socket, reconnect).
//! - §4.4: `nudge` (scope `desired-state`) triggers an immediate
//!   fetch-and-converge cycle, rate-limited (≥ 5 s between
//!   nudge-triggered cycles, bursts coalesced) and MUST NOT alter the
//!   polling schedule — polling stays the correctness path (Law 5).
//! - §4.5: jittered exponential backoff — base 1 s, factor 2, full
//!   jitter, cap 5 min; uptime ≥ 60 s resets. In-memory only:
//!   restart begins fresh at base (Law 3 — startup IS recovery, no
//!   persisted reconnect state exists to corrupt).
//! - §4.6: channel absence changes NOTHING about convergence. This
//!   module owns no converge state; it only feeds a nudge signal into
//!   the main loop's select.
//!
//! Integration seam (docs/build-charter.md CODE BOUNDARY): core never
//! calls this module. The binary shell spawns [`spawn`] and replaces
//! its inter-cycle sleep with [`next_cycle`]; with the feature off
//! (or the channel never connecting) B1–B4 behavior is identical.
//!
//! Sub-channel consumer seam: ext-terminal (REV-002, B6) registers a
//! [`SubChannelHandler`] for purpose `rev-002/terminal` on the
//! [`SubChannelRegistry`] passed to [`spawn`]. ext-terminal depends
//! on ext-channel in the feature graph — any extension needing
//! bidirectional bytes MUST ride a sub-channel, never a second
//! socket (§4.2).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reeve_types::reeve::capabilities::{ServerCapabilities, parse_extension, rev};
use reeve_types::reeve::channel::{
    CHANNEL_PATH, CHANNEL_PROTOCOL, ControlFrame, KEEPALIVE_IDLE_SECS, MAX_FRAME_BYTES,
    MAX_SUB_CHANNELS, NUDGE_SCOPE_DESIRED_STATE, PONG_TIMEOUT_SECS, decode_data_frame,
    encode_data_frame, server_allocated,
};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::source::CAPABILITIES_PATH;

/// Minimum gap between nudge-triggered cycles
/// (spec/reeve/02-channel.md §4.4 RECOMMENDED).
pub const NUDGE_MIN_GAP: Duration = Duration::from_secs(5);
/// Reconnect backoff base (§4.5 RECOMMENDED).
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Reconnect backoff cap (§4.5 RECOMMENDED).
pub const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// A channel that lived at least this long resets backoff (§4.5).
pub const UPTIME_RESET: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------
// Sub-channel consumer seam (B6 and later extensions plug in here)
// ---------------------------------------------------------------

/// Registered per purpose; called when the SERVER opens a sub-channel
/// with that purpose (§4.2). Agent-initiated opens (odd ids,
/// [`AgentIds`]) land with the first extension that needs them.
pub trait SubChannelHandler: Send + Sync {
    /// Accept (`Ok`) or refuse (`Err(reason)` → `reject`) a
    /// server-opened sub-channel. `tx` sends data/close frames back;
    /// it is the handler's to keep for the sub-channel's lifetime.
    fn open(
        &self,
        id: u32,
        meta: Option<serde_json::Value>,
        tx: SubChannelTx,
    ) -> Result<Box<dyn SubChannelConsumer>, String>;
}

/// One live sub-channel's receive side, owned by the channel task.
pub trait SubChannelConsumer: Send {
    /// A data frame arrived for this sub-channel (payload only — the
    /// 4-byte id prefix is already stripped).
    fn data(&mut self, payload: &[u8]);
    /// The sub-channel closed — peer `close` frame or whole-channel
    /// teardown. A normal event, not corruption (§4.2).
    fn closed(&mut self);
}

/// Purpose → handler registry, fixed at [`spawn`] time.
#[derive(Clone, Default)]
pub struct SubChannelRegistry {
    handlers: BTreeMap<String, Arc<dyn SubChannelHandler>>,
}

impl SubChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `handler` for a purpose (e.g. `rev-002/terminal`).
    pub fn register(&mut self, purpose: &str, handler: Arc<dyn SubChannelHandler>) {
        self.handlers.insert(purpose.to_string(), handler);
    }

    /// Purposes supported — advertised in our `hello.extensions`.
    pub fn purposes(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }

    fn get(&self, purpose: &str) -> Option<&Arc<dyn SubChannelHandler>> {
        self.handlers.get(purpose)
    }
}

/// What the channel task sends toward the socket on behalf of
/// sub-channel handlers.
pub(crate) enum Outgoing {
    Frame(Message),
    /// Handler-initiated close: the channel task drops the consumer
    /// AND emits the `close` control frame.
    CloseSub { id: u32, reason: Option<String> },
}

/// A sub-channel's send side, handed to the handler at open.
#[derive(Clone)]
pub struct SubChannelTx {
    id: u32,
    out: mpsc::Sender<Outgoing>,
}

impl SubChannelTx {
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Send one data frame. `false` when the channel is gone —
    /// treat as [`SubChannelConsumer::closed`] pending (§4.2).
    pub async fn send(&self, payload: &[u8]) -> bool {
        self.out
            .send(Outgoing::Frame(Message::Binary(
                encode_data_frame(self.id, payload).into(),
            )))
            .await
            .is_ok()
    }

    /// Close this sub-channel from our side.
    pub async fn close(&self, reason: Option<String>) -> bool {
        self.out
            .send(Outgoing::CloseSub { id: self.id, reason })
            .await
            .is_ok()
    }
}

/// In-crate test seam: a [`SubChannelTx`] wired to an observable
/// pipe instead of a live channel task (used by ext::terminal's
/// session tests).
#[cfg(test)]
pub(crate) fn test_sub_channel(id: u32) -> (SubChannelTx, mpsc::Receiver<Outgoing>) {
    let (out, rx) = mpsc::channel(64);
    (SubChannelTx { id, out }, rx)
}

/// Agent-side sub-channel id allocator — odd ids only (§4.2:
/// allocation never collides with the server's even space).
#[derive(Debug, Default)]
pub struct AgentIds {
    next: u32,
}

impl AgentIds {
    pub fn new() -> Self {
        AgentIds { next: 1 }
    }

    pub fn next_id(&mut self) -> u32 {
        // Start at 1 even when Default-constructed with next == 0.
        self.next |= 1;
        let id = self.next;
        self.next = self.next.wrapping_add(2);
        id
    }
}

// ---------------------------------------------------------------
// Nudge rate limiting (§4.4)
// ---------------------------------------------------------------

/// At most one nudge-triggered cycle per [`NUDGE_MIN_GAP`]; bursts
/// coalesce (§4.4). Pure decision logic, unit-tested.
#[derive(Debug)]
pub struct NudgeLimiter {
    min_gap: Duration,
    last: Option<Instant>,
}

impl NudgeLimiter {
    pub fn new() -> Self {
        Self::with_gap(NUDGE_MIN_GAP)
    }

    /// Custom gap — tests use milliseconds.
    pub fn with_gap(min_gap: Duration) -> Self {
        NudgeLimiter { min_gap, last: None }
    }

    /// How long a nudge arriving at `now` must wait before its cycle
    /// may run. Zero when no nudge-triggered cycle ran recently.
    pub fn delay(&self, now: Instant) -> Duration {
        match self.last {
            None => Duration::ZERO,
            Some(last) => (last + self.min_gap).saturating_duration_since(now),
        }
    }

    /// Record that a nudge-triggered cycle ran at `now`.
    pub fn mark(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

impl Default for NudgeLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Reconnect backoff (§4.5)
// ---------------------------------------------------------------

/// Jittered exponential backoff: full jitter over
/// `min(cap, base * 2^attempt)`. In-memory only (§4.5, Law 3).
#[derive(Debug)]
pub struct Backoff {
    attempt: u32,
    base: Duration,
    cap: Duration,
    rng: u64,
}

impl Backoff {
    pub fn new() -> Self {
        Self::with_params(BACKOFF_BASE, BACKOFF_CAP)
    }

    pub fn with_params(base: Duration, cap: Duration) -> Self {
        Backoff {
            attempt: 0,
            base,
            cap,
            // Seed from wall clock; quality is irrelevant — jitter
            // only needs to decorrelate a reconnect storm (§4.5).
            rng: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
                | 1,
        }
    }

    /// The ceiling the next delay is drawn from (exposed for tests).
    pub fn ceiling(&self) -> Duration {
        let base_ms = self.base.as_millis() as u64;
        let ceil_ms = base_ms
            .checked_shl(self.attempt)
            .unwrap_or(u64::MAX)
            .min(self.cap.as_millis() as u64);
        Duration::from_millis(ceil_ms)
    }

    /// Next delay: uniform in `[0, ceiling]` (full jitter), then the
    /// ceiling doubles.
    pub fn next_delay(&mut self) -> Duration {
        let ceil_ms = self.ceiling().as_millis() as u64;
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(xorshift64(&mut self.rng) % (ceil_ms + 1))
    }

    /// Back to base — a connection lived ≥ [`UPTIME_RESET`] (§4.5).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

// ---------------------------------------------------------------
// Channel runtime — what the binary shell holds
// ---------------------------------------------------------------

/// What [`next_cycle`] returned: a scheduled poll tick or a
/// nudge-triggered immediate cycle (§4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleTrigger {
    Poll,
    Nudge,
}

/// Handle the binary shell keeps: the nudge signal out of the
/// channel task, plus its rate limiter.
pub struct ChannelRuntime {
    /// Capacity-1: a burst of nudges while one is pending coalesces
    /// to a single signal (§4.4).
    pub nudges: mpsc::Receiver<()>,
    pub limiter: NudgeLimiter,
}

/// Wait for the next cycle: the poll interval tick (the correctness
/// path — NEVER lengthened, suspended, or skipped because the
/// channel is open, §4.4) or a rate-limited nudge. A nudge inside
/// the rate-limit window waits out the remainder, coalescing further
/// nudges, then triggers one cycle. Since a nudge-triggered cycle IS
/// a full fetch-and-converge, the gap between consecutive polls
/// never exceeds `interval`.
pub async fn next_cycle(interval: Duration, ch: &mut ChannelRuntime) -> CycleTrigger {
    tokio::select! {
        _ = tokio::time::sleep(interval) => CycleTrigger::Poll,
        Some(()) = ch.nudges.recv() => {
            let wait = ch.limiter.delay(Instant::now());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
                // Coalesce anything that queued during the wait.
                while ch.nudges.try_recv().is_ok() {}
            }
            ch.limiter.mark(Instant::now());
            CycleTrigger::Nudge
        }
    }
}

/// Spawn the channel task. `None` when there is no channel to have:
/// `dir://` sources have no server, and an unenrolled agent has no
/// device credential to upgrade with (§4.1). Never blocks — startup,
/// enrollment, and first converge never wait on the channel (§4.6).
pub fn spawn(
    server: &str,
    device_token: Option<String>,
    registry: SubChannelRegistry,
) -> Option<ChannelRuntime> {
    if !(server.starts_with("https://") || server.starts_with("http://")) {
        return None;
    }
    let token = device_token?;
    let cfg = ChannelConfig {
        base: server.trim_end_matches('/').to_string(),
        token,
    };
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(run(cfg, registry, tx));
    Some(ChannelRuntime {
        nudges: rx,
        limiter: NudgeLimiter::new(),
    })
}

struct ChannelConfig {
    base: String,
    token: String,
}

/// Keepalive knobs (§4.3) — shortened by tests.
#[derive(Clone, Copy)]
struct Timings {
    idle: Duration,
    pong_timeout: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            idle: Duration::from_secs(KEEPALIVE_IDLE_SECS),
            pong_timeout: Duration::from_secs(PONG_TIMEOUT_SECS),
        }
    }
}

/// The forever task: probe → connect → serve → backoff → repeat.
/// Failure is feature-unavailable, logged once per outage streak;
/// polling continues untouched in the main loop (§4.1, §4.6).
async fn run(cfg: ChannelConfig, registry: SubChannelRegistry, nudges: mpsc::Sender<()>) {
    let probe = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("static reqwest client config");
    let mut backoff = Backoff::new();
    let mut outage_logged = false;
    loop {
        // §4.1: MUST NOT attempt the channel unless the server
        // advertises rev-001/1. Probed per attempt (not once at
        // startup) so a server that comes up — or upgrades — after
        // us is discovered on the same backoff schedule.
        if !server_advertises_channel(&probe, &cfg).await {
            if !outage_logged {
                info!(
                    "server does not advertise rev-001/1 (or is unreachable); \
                     channel off, polling continues (spec/reeve/02-channel.md §4.1)"
                );
                outage_logged = true;
            }
            tokio::time::sleep(backoff.next_delay()).await;
            continue;
        }
        let started = Instant::now();
        match connect_once(&cfg, &registry, &nudges, Timings::default()).await {
            Ok(()) => info!("channel closed by peer; reconnecting (§4.5)"),
            Err(e) if !outage_logged => {
                // Log once (§4.1); the retry loop itself is silent.
                warn!(error = %e, "channel unavailable; polling continues (§4.1)");
                outage_logged = true;
            }
            Err(e) => debug!(error = %e, "channel attempt failed"),
        }
        if started.elapsed() >= UPTIME_RESET {
            backoff.reset();
            outage_logged = false;
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

/// True iff the server advertises `rev-001/1`
/// (spec/reeve/01-framework.md §3.3; 02-channel §4.1). Any error —
/// unreachable, 404, unparseable — is "not advertised".
async fn server_advertises_channel(client: &reqwest::Client, cfg: &ChannelConfig) -> bool {
    let resp = match client
        .get(format!("{}{CAPABILITIES_PATH}", cfg.base))
        .bearer_auth(&cfg.token)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return false,
    };
    match resp.json::<ServerCapabilities>().await {
        Ok(caps) => channel_advertised(&caps),
        Err(_) => false,
    }
}

/// Exact-version check: `rev-001/1` (§3.4 — versions are distinct
/// capabilities, not ranges).
pub fn channel_advertised(caps: &ServerCapabilities) -> bool {
    caps.extensions
        .iter()
        .filter_map(|e| parse_extension(e))
        .any(|(r, v)| r == rev::CHANNEL && v == 1)
}

/// `https://…` → `wss://…/api/reeve/v1/channel` (same TLS listener
/// as everything else, §4.1); `http://` → `ws://` for tests.
fn ws_url(base: &str) -> Result<String, String> {
    if let Some(rest) = base.strip_prefix("https://") {
        Ok(format!("wss://{rest}{CHANNEL_PATH}"))
    } else if let Some(rest) = base.strip_prefix("http://") {
        Ok(format!("ws://{rest}{CHANNEL_PATH}"))
    } else {
        Err(format!("channel needs an http(s) server, got {base:?}"))
    }
}

/// One connection attempt: upgrade, hello, then serve until the
/// channel dies. `Ok(())` = peer closed cleanly; `Err` = upgrade or
/// liveness failure. Either way the caller backs off and retries.
async fn connect_once(
    cfg: &ChannelConfig,
    registry: &SubChannelRegistry,
    nudges: &mpsc::Sender<()>,
    timings: Timings,
) -> anyhow::Result<()> {
    let mut request = ws_url(&cfg.base)
        .map_err(anyhow::Error::msg)?
        .into_client_request()?;
    // Same device credential as the device API (§4.1, §4.7).
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {}", cfg.token).parse()?,
    );
    // §4.7 resource limits, applied on our side too.
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    let (ws, _resp) =
        tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false).await?;
    info!("channel open (rev-001/1)");
    serve(ws, registry, nudges, timings).await
}

fn control_msg(frame: &ControlFrame) -> anyhow::Result<Message> {
    Ok(Message::Text(serde_json::to_string(frame)?.into()))
}

/// The per-connection actor: hello, then select over incoming
/// frames, sub-channel output, and the keepalive timer.
async fn serve(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    registry: &SubChannelRegistry,
    nudges: &mpsc::Sender<()>,
    timings: Timings,
) -> anyhow::Result<()> {
    let (mut sink, mut stream) = ws.split();
    // Sub-channel handlers send through this; kept open locally so
    // recv() can't starve.
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(64);
    let mut subs: BTreeMap<u32, Box<dyn SubChannelConsumer>> = BTreeMap::new();
    let mut rng = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xDEAD_BEEF_CAFE_F00D)
        | 1;

    // hello, once at open (§4.2): protocol + purposes we support.
    sink.send(control_msg(&ControlFrame::Hello {
        protocol: CHANNEL_PROTOCOL.to_string(),
        extensions: registry.purposes(),
    })?)
    .await?;

    let mut last_rx = Instant::now();
    // Some(nonce, deadline) while a ping awaits its pong (§4.3).
    let mut pending_pong: Option<(String, Instant)> = None;

    let result: anyhow::Result<()> = loop {
        let deadline = match &pending_pong {
            Some((_, d)) => *d,
            None => last_rx + timings.idle,
        };
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    None => break Ok(()), // peer hung up
                    Some(Err(e)) => break Err(e.into()),
                    Some(Ok(m)) => m,
                };
                last_rx = Instant::now();
                match msg {
                    Message::Text(text) => {
                        // Unknown `type` deserializes to Unknown and
                        // is ignored below (§4.2); malformed JSON is
                        // ignored the same way (tolerant reader,
                        // 01-framework §3.4).
                        let frame = match serde_json::from_str::<ControlFrame>(text.as_str()) {
                            Ok(f) => f,
                            Err(e) => {
                                debug!(error = %e, "unparseable control frame ignored");
                                continue;
                            }
                        };
                        match frame {
                            ControlFrame::Hello { protocol, extensions } => {
                                debug!(%protocol, ?extensions, "server hello");
                            }
                            ControlFrame::Nudge { scope, .. } => {
                                if scope == NUDGE_SCOPE_DESIRED_STATE {
                                    // Capacity-1 try_send: bursts
                                    // coalesce; delivery is
                                    // best-effort by spec (§4.4).
                                    let _ = nudges.try_send(());
                                } else {
                                    debug!(%scope, "nudge scope not handled; ignored");
                                }
                            }
                            ControlFrame::Ping { nonce } => {
                                sink.send(control_msg(&ControlFrame::Pong { nonce })?).await?;
                            }
                            ControlFrame::Pong { nonce } => {
                                match &pending_pong {
                                    Some((expect, _)) if *expect == nonce => pending_pong = None,
                                    _ => debug!(%nonce, "unsolicited pong ignored"),
                                }
                            }
                            ControlFrame::Open { id, purpose, meta } => {
                                let response = handle_open(
                                    id, &purpose, meta, registry, &mut subs, &out_tx,
                                );
                                sink.send(control_msg(&response)?).await?;
                            }
                            ControlFrame::Accept { id } | ControlFrame::Reject { id, .. } => {
                                // Agent-initiated opens land with the
                                // first extension needing them (B6+);
                                // until then nothing is pending.
                                debug!(id, "accept/reject with no pending open; ignored");
                            }
                            ControlFrame::Close { id, .. } => {
                                if let Some(mut consumer) = subs.remove(&id) {
                                    consumer.closed();
                                }
                                // Unknown id: frames race close —
                                // silent (§4.2).
                            }
                            ControlFrame::Unknown => {} // §4.2: MUST ignore
                        }
                    }
                    Message::Binary(data) => {
                        // Route by sub-channel id; anything not
                        // accepted-and-open is discarded silently
                        // (§4.2 — frames race close).
                        if let Some((id, payload)) = decode_data_frame(&data)
                            && let Some(consumer) = subs.get_mut(&id)
                        {
                            consumer.data(payload);
                        }
                    }
                    Message::Close(_) => break Ok(()),
                    // Transport-level ping/pong: tungstenite queues
                    // the reply itself; both already refreshed
                    // last_rx above.
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            out = out_rx.recv() => {
                match out.expect("out_tx held by serve") {
                    Outgoing::Frame(m) => sink.send(m).await?,
                    Outgoing::CloseSub { id, reason } => {
                        if let Some(mut consumer) = subs.remove(&id) {
                            consumer.closed();
                        }
                        sink.send(control_msg(&ControlFrame::Close { id, reason })?).await?;
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                match pending_pong.take() {
                    Some((nonce, _)) => {
                        // §4.3: missing pong within the timeout is a
                        // dead channel — close, reconnect.
                        break Err(anyhow::anyhow!(
                            "keepalive: no pong for nonce {nonce} within {:?}",
                            timings.pong_timeout
                        ));
                    }
                    None => {
                        let nonce = format!("{:016x}", xorshift64(&mut rng));
                        sink.send(control_msg(&ControlFrame::Ping { nonce: nonce.clone() })?)
                            .await?;
                        pending_pong = Some((nonce, Instant::now() + timings.pong_timeout));
                    }
                }
            }
        }
    };

    // Channel teardown implicitly closes all sub-channels — a normal
    // event, not corruption (§4.2).
    for (_, mut consumer) in subs {
        consumer.closed();
    }
    result
}

/// Decide `accept` or `reject` for a server-sent `open` (§4.2).
fn handle_open(
    id: u32,
    purpose: &str,
    meta: Option<serde_json::Value>,
    registry: &SubChannelRegistry,
    subs: &mut BTreeMap<u32, Box<dyn SubChannelConsumer>>,
    out_tx: &mpsc::Sender<Outgoing>,
) -> ControlFrame {
    let reject = |reason: String| ControlFrame::Reject { id, reason };
    if !server_allocated(id) {
        return reject("server-opened sub-channel ids must be even".into());
    }
    if subs.contains_key(&id) {
        return reject(format!("sub-channel id {id} already open"));
    }
    if subs.len() >= MAX_SUB_CHANNELS {
        return reject(format!("sub-channel cap {MAX_SUB_CHANNELS} reached"));
    }
    let Some(handler) = registry.get(purpose) else {
        // §4.2: unsupported purpose => reject, never teardown.
        return reject(format!("unsupported purpose {purpose:?}"));
    };
    let tx = SubChannelTx {
        id,
        out: out_tx.clone(),
    };
    match handler.open(id, meta, tx) {
        Ok(consumer) => {
            subs.insert(id, consumer);
            ControlFrame::Accept { id }
        }
        Err(reason) => reject(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::extract::ws::{Message as WsMsg, WebSocket, WebSocketUpgrade};
    use axum::http::HeaderMap;
    use axum::response::Response;
    use axum::routing::{any, get};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    type WsScript =
        Arc<dyn Fn(WebSocket) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

    #[derive(Clone)]
    struct MockState {
        script: WsScript,
        advertise: bool,
    }

    /// Mock reeve server: capabilities endpoint + channel upgrade
    /// (asserting the device bearer token, §4.1), driving `script`
    /// per connection.
    async fn mock_server<F, Fut>(advertise: bool, script: F) -> String
    where
        F: Fn(WebSocket) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let script: WsScript = Arc::new(move |ws| Box::pin(script(ws)));
        async fn caps(State(s): State<MockState>) -> axum::Json<ServerCapabilities> {
            axum::Json(ServerCapabilities {
                server_version: "test".into(),
                extensions: if s.advertise {
                    vec!["rev-001/1".into()]
                } else {
                    vec![]
                },
            })
        }
        async fn channel(
            State(s): State<MockState>,
            headers: HeaderMap,
            ws: WebSocketUpgrade,
        ) -> Response {
            assert_eq!(
                headers.get("authorization").and_then(|v| v.to_str().ok()),
                Some("Bearer tok-dev-1"),
                "upgrade must carry the device bearer token (§4.1)"
            );
            let script = s.script.clone();
            ws.on_upgrade(move |sock| (script)(sock))
        }
        let app = Router::new()
            .route(CAPABILITIES_PATH, get(caps))
            .route(CHANNEL_PATH, any(channel))
            .with_state(MockState { script, advertise });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn cfg(base: &str) -> ChannelConfig {
        ChannelConfig {
            base: base.to_string(),
            token: "tok-dev-1".into(),
        }
    }

    async fn recv_control(ws: &mut WebSocket) -> ControlFrame {
        loop {
            match ws.recv().await.expect("socket open").expect("frame") {
                WsMsg::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
                WsMsg::Binary(_) => panic!("expected control frame"),
                _ => continue, // transport ping/pong
            }
        }
    }

    async fn recv_binary(ws: &mut WebSocket) -> Vec<u8> {
        loop {
            match ws.recv().await.expect("socket open").expect("frame") {
                WsMsg::Binary(b) => return b.to_vec(),
                WsMsg::Text(t) => panic!("expected data frame, got control {t}"),
                _ => continue,
            }
        }
    }

    async fn send_control(ws: &mut WebSocket, frame: &ControlFrame) {
        ws.send(WsMsg::Text(
            serde_json::to_string(frame).unwrap().into(),
        ))
        .await
        .unwrap();
    }

    fn nudge_pipe() -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
        mpsc::channel(1)
    }

    // ---- hello + nudge -------------------------------------------------

    #[tokio::test]
    async fn hello_exchange_then_nudge_reaches_the_loop() {
        let base = mock_server(true, |mut ws| async move {
            // Agent speaks hello first, once, with the protocol id.
            let hello = recv_control(&mut ws).await;
            let ControlFrame::Hello { protocol, .. } = hello else {
                panic!("first frame must be hello, got {hello:?}");
            };
            assert_eq!(protocol, CHANNEL_PROTOCOL);
            // Server hello back (both-ways once, §4.2).
            send_control(
                &mut ws,
                &ControlFrame::Hello {
                    protocol: CHANNEL_PROTOCOL.into(),
                    extensions: vec![],
                },
            )
            .await;
            send_control(
                &mut ws,
                &ControlFrame::Nudge {
                    scope: NUDGE_SCOPE_DESIRED_STATE.into(),
                    hint: None,
                },
            )
            .await;
            // Hold the socket open while the test observes the nudge.
            let _ = ws.recv().await;
        })
        .await;

        // Full spawn path: probe gate + upgrade + serve.
        let mut runtime = spawn(&base, Some("tok-dev-1".into()), SubChannelRegistry::new())
            .expect("http + token => channel runtime");
        let got = tokio::time::timeout(Duration::from_secs(10), runtime.nudges.recv())
            .await
            .expect("nudge within timeout");
        assert_eq!(got, Some(()));
    }

    #[tokio::test]
    async fn spawn_declines_dir_sources_and_missing_tokens() {
        // §4.6: no server / no credential => no channel, no error.
        assert!(spawn("dir:///opt/src", Some("t".into()), SubChannelRegistry::new()).is_none());
        assert!(spawn("http://127.0.0.1:1", None, SubChannelRegistry::new()).is_none());
    }

    // ---- capability gate (§4.1) ----------------------------------------

    #[tokio::test]
    async fn no_attempt_unless_rev001_advertised() {
        let upgrades = Arc::new(Mutex::new(0u32));
        let seen = upgrades.clone();
        let base = mock_server(false, move |_ws| {
            *seen.lock().unwrap() += 1;
            async {}
        })
        .await;
        let client = reqwest::Client::new();
        assert!(
            !server_advertises_channel(&client, &cfg(&base)).await,
            "empty extension list must gate the channel off"
        );
        // Unreachable server: also "not advertised", never an error.
        assert!(!server_advertises_channel(&client, &cfg("http://127.0.0.1:1")).await);
        assert_eq!(*upgrades.lock().unwrap(), 0, "no upgrade attempted");

        let base = mock_server(true, |_ws| async {}).await;
        assert!(server_advertises_channel(&client, &cfg(&base)).await);
    }

    #[test]
    fn channel_advertised_is_exact_version() {
        let caps = |exts: &[&str]| ServerCapabilities {
            server_version: "t".into(),
            extensions: exts.iter().map(|s| s.to_string()).collect(),
        };
        assert!(channel_advertised(&caps(&["rev-001/1", "rev-009/1"])));
        assert!(!channel_advertised(&caps(&[])));
        assert!(!channel_advertised(&caps(&["rev-001/2"])), "v2-only is not v1");
        assert!(!channel_advertised(&caps(&["rev-002/1", "junk"])));
    }

    // ---- keepalive (§4.3) ----------------------------------------------

    #[tokio::test]
    async fn missing_pong_closes_the_socket() {
        let base = mock_server(true, |mut ws| async move {
            let _hello = recv_control(&mut ws).await;
            // Go silent: never answer the agent's ping.
            while let Some(Ok(_)) = ws.recv().await {}
        })
        .await;
        let (tx, _rx) = nudge_pipe();
        let timings = Timings {
            idle: Duration::from_millis(100),
            pong_timeout: Duration::from_millis(100),
        };
        let started = Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            connect_once(&cfg(&base), &SubChannelRegistry::new(), &tx, timings),
        )
        .await
        .expect("must not hang")
        .expect_err("silent peer must be declared dead");
        assert!(err.to_string().contains("no pong"), "{err}");
        assert!(started.elapsed() >= Duration::from_millis(200), "idle + pong timeout");
    }

    #[tokio::test]
    async fn agent_answers_peer_ping_with_matching_nonce() {
        let base = mock_server(true, |mut ws| async move {
            let _hello = recv_control(&mut ws).await;
            send_control(&mut ws, &ControlFrame::Ping { nonce: "srv-7".into() }).await;
            let pong = recv_control(&mut ws).await;
            assert_eq!(pong, ControlFrame::Pong { nonce: "srv-7".into() });
            // Signal success to the test through the nudge path.
            send_control(
                &mut ws,
                &ControlFrame::Nudge {
                    scope: NUDGE_SCOPE_DESIRED_STATE.into(),
                    hint: None,
                },
            )
            .await;
            let _ = ws.recv().await;
        })
        .await;
        let (tx, mut rx) = nudge_pipe();
        let registry = SubChannelRegistry::new();
        let task = tokio::spawn(async move {
            let _ = connect_once(&cfg(&base), &registry, &tx, Timings::default()).await;
        });
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;
        assert_eq!(got.expect("pong accepted => nudge follows"), Some(()));
        task.abort();
    }

    // ---- unknown control types (§4.2) ----------------------------------

    #[tokio::test]
    async fn unknown_control_type_is_ignored_channel_stays_up() {
        let base = mock_server(true, |mut ws| async move {
            let _hello = recv_control(&mut ws).await;
            ws.send(WsMsg::Text(
                r#"{"type": "frobnicate", "with": ["fields"]}"#.into(),
            ))
            .await
            .unwrap();
            ws.send(WsMsg::Text("not even json".into())).await.unwrap();
            // Channel must still be alive: nudge goes through.
            send_control(
                &mut ws,
                &ControlFrame::Nudge {
                    scope: NUDGE_SCOPE_DESIRED_STATE.into(),
                    hint: Some(serde_json::json!({"v": 3})),
                },
            )
            .await;
            let _ = ws.recv().await;
        })
        .await;
        let (tx, mut rx) = nudge_pipe();
        let registry = SubChannelRegistry::new();
        let task = tokio::spawn(async move {
            let _ = connect_once(&cfg(&base), &registry, &tx, Timings::default()).await;
        });
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;
        assert_eq!(got.expect("channel survived unknown frames"), Some(()));
        task.abort();
    }

    // ---- sub-channels (§4.2) -------------------------------------------

    #[derive(Default)]
    struct SubLog {
        data: Vec<Vec<u8>>,
        closed: bool,
    }

    struct RecordingHandler {
        log: Arc<Mutex<SubLog>>,
        /// Bytes to send back through the tx at open (exercises
        /// SubChannelTx routing).
        greet: Vec<u8>,
    }

    struct RecordingConsumer {
        log: Arc<Mutex<SubLog>>,
    }

    impl SubChannelHandler for RecordingHandler {
        fn open(
            &self,
            _id: u32,
            _meta: Option<serde_json::Value>,
            tx: SubChannelTx,
        ) -> Result<Box<dyn SubChannelConsumer>, String> {
            let greet = self.greet.clone();
            tokio::spawn(async move {
                tx.send(&greet).await;
            });
            Ok(Box::new(RecordingConsumer {
                log: self.log.clone(),
            }))
        }
    }

    impl SubChannelConsumer for RecordingConsumer {
        fn data(&mut self, payload: &[u8]) {
            self.log.lock().unwrap().data.push(payload.to_vec());
        }
        fn closed(&mut self) {
            self.log.lock().unwrap().closed = true;
        }
    }

    #[tokio::test]
    async fn sub_channel_open_reject_data_routing_and_close() {
        let log: Arc<Mutex<SubLog>> = Arc::default();
        let mut registry = SubChannelRegistry::new();
        registry.register(
            reeve_types::reeve::channel::PURPOSE_TERMINAL,
            Arc::new(RecordingHandler {
                log: log.clone(),
                greet: b"greet-from-agent".to_vec(),
            }),
        );
        assert_eq!(registry.purposes(), vec!["rev-002/terminal".to_string()]);

        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let done_tx = Mutex::new(Some(done_tx));
        let base = mock_server(true, move |mut ws| {
            let done = done_tx.lock().unwrap().take().unwrap();
            async move {
                let hello = recv_control(&mut ws).await;
                assert_eq!(
                    hello,
                    ControlFrame::Hello {
                        protocol: CHANNEL_PROTOCOL.into(),
                        extensions: vec!["rev-002/terminal".into()],
                    },
                    "hello advertises registered purposes"
                );

                // Supported purpose, even id => accept.
                send_control(
                    &mut ws,
                    &ControlFrame::Open {
                        id: 2,
                        purpose: "rev-002/terminal".into(),
                        meta: None,
                    },
                )
                .await;
                assert_eq!(recv_control(&mut ws).await, ControlFrame::Accept { id: 2 });
                // Handler's greeting rides the sub-channel back.
                assert_eq!(
                    recv_binary(&mut ws).await,
                    encode_data_frame(2, b"greet-from-agent")
                );

                // Unsupported purpose => reject, channel intact.
                send_control(
                    &mut ws,
                    &ControlFrame::Open {
                        id: 4,
                        purpose: "rev-999/nope".into(),
                        meta: None,
                    },
                )
                .await;
                let ControlFrame::Reject { id: 4, .. } = recv_control(&mut ws).await else {
                    panic!("unsupported purpose must be rejected (§4.2)");
                };

                // Odd (agent-space) id from the server => reject.
                send_control(
                    &mut ws,
                    &ControlFrame::Open {
                        id: 3,
                        purpose: "rev-002/terminal".into(),
                        meta: None,
                    },
                )
                .await;
                let ControlFrame::Reject { id: 3, .. } = recv_control(&mut ws).await else {
                    panic!("server ids are even (§4.2)");
                };

                // Data to the open sub-channel: routed.
                ws.send(WsMsg::Binary(encode_data_frame(2, b"to-agent").into()))
                    .await
                    .unwrap();
                // Data to a NON-open id: discarded silently.
                ws.send(WsMsg::Binary(encode_data_frame(8, b"lost").into()))
                    .await
                    .unwrap();
                // Runt frame (< 4 bytes): discarded silently.
                ws.send(WsMsg::Binary(vec![0, 1].into())).await.unwrap();

                // close => consumer.closed(); further data discarded.
                send_control(&mut ws, &ControlFrame::Close { id: 2, reason: None }).await;
                ws.send(WsMsg::Binary(encode_data_frame(2, b"after-close").into()))
                    .await
                    .unwrap();

                // Sync point: a ping the agent must answer proves all
                // the above was processed in order.
                send_control(&mut ws, &ControlFrame::Ping { nonce: "sync".into() }).await;
                assert_eq!(
                    recv_control(&mut ws).await,
                    ControlFrame::Pong { nonce: "sync".into() }
                );
                let _ = done.send(());
                let _ = ws.recv().await;
            }
        })
        .await;

        let (tx, _rx) = nudge_pipe();
        let task = tokio::spawn(async move {
            let _ = connect_once(&cfg(&base), &registry, &tx, Timings::default()).await;
        });
        tokio::time::timeout(Duration::from_secs(10), done_rx)
            .await
            .expect("script finished")
            .unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.data, vec![b"to-agent".to_vec()], "only open-id data routed");
        assert!(log.closed, "close frame reached the consumer");
        task.abort();
    }

    #[test]
    fn agent_id_allocator_is_odd_only() {
        let mut ids = AgentIds::new();
        assert_eq!(ids.next_id(), 1);
        assert_eq!(ids.next_id(), 3);
        let mut defaulted = AgentIds::default();
        assert_eq!(defaulted.next_id() % 2, 1, "default starts odd too");
    }

    // ---- nudge rate limit (§4.4) ---------------------------------------

    #[tokio::test(start_paused = true)]
    async fn nudge_limiter_delay_math() {
        let mut l = NudgeLimiter::new();
        let t0 = Instant::now();
        assert_eq!(l.delay(t0), Duration::ZERO, "first nudge runs immediately");
        l.mark(t0);
        assert_eq!(l.delay(t0 + Duration::from_secs(1)), Duration::from_secs(4));
        assert_eq!(l.delay(t0 + Duration::from_secs(5)), Duration::ZERO);
        assert_eq!(l.delay(t0 + Duration::from_secs(60)), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn next_cycle_poll_tick_without_nudges() {
        let (_tx, rx) = nudge_pipe();
        let mut rt = ChannelRuntime {
            nudges: rx,
            limiter: NudgeLimiter::new(),
        };
        let t0 = Instant::now();
        let got = next_cycle(Duration::from_secs(30), &mut rt).await;
        assert_eq!(got, CycleTrigger::Poll);
        assert_eq!(Instant::now() - t0, Duration::from_secs(30), "full interval slept");
    }

    #[tokio::test(start_paused = true)]
    async fn nudge_triggers_immediate_cycle_then_rate_limits_and_coalesces() {
        let (tx, rx) = nudge_pipe();
        let mut rt = ChannelRuntime {
            nudges: rx,
            limiter: NudgeLimiter::new(),
        };
        let interval = Duration::from_secs(30);

        // First nudge: immediate (no prior nudge cycle).
        tx.try_send(()).unwrap();
        let t0 = Instant::now();
        assert_eq!(next_cycle(interval, &mut rt).await, CycleTrigger::Nudge);
        assert!(Instant::now() - t0 < Duration::from_secs(1), "no rate-limit wait");

        // Burst of nudges right after: capacity-1 channel keeps one.
        tx.try_send(()).unwrap();
        assert!(tx.try_send(()).is_err(), "burst coalesces in the capacity-1 pipe");

        // Second nudge cycle waits out the 5s gap (not the 30s
        // interval — nudges shorten latency; §4.4).
        let t1 = Instant::now();
        assert_eq!(next_cycle(interval, &mut rt).await, CycleTrigger::Nudge);
        let waited = Instant::now() - t1;
        assert!(
            waited >= Duration::from_secs(4) && waited < Duration::from_secs(6),
            "rate-limited to the 5s gap, got {waited:?}"
        );

        // Nothing pending afterwards: the poll tick is untouched.
        let t2 = Instant::now();
        assert_eq!(next_cycle(interval, &mut rt).await, CycleTrigger::Poll);
        assert_eq!(Instant::now() - t2, interval, "polling schedule never altered");
    }

    // ---- backoff (§4.5) --------------------------------------------------

    #[test]
    fn backoff_full_jitter_bounds_and_reset() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(300);
        let mut b = Backoff::with_params(base, cap);
        // Ceilings double from base and clamp at the cap.
        let expected_ceilings: Vec<u64> = (0..12)
            .map(|n| (1000u64 << n).min(300_000))
            .collect();
        for (attempt, want_ceil) in expected_ceilings.iter().enumerate() {
            assert_eq!(
                b.ceiling().as_millis() as u64,
                *want_ceil,
                "ceiling at attempt {attempt}"
            );
            let d = b.next_delay();
            assert!(
                d.as_millis() as u64 <= *want_ceil,
                "full jitter stays under the ceiling at attempt {attempt}: {d:?}"
            );
        }
        // Deep attempts stay clamped at the cap (no shl overflow).
        for _ in 0..100 {
            assert!(b.next_delay() <= cap);
        }
        assert_eq!(b.ceiling(), cap);
        // Reset returns to base (§4.5: uptime >= 60s resets).
        b.reset();
        assert_eq!(b.ceiling(), base);
        assert!(b.next_delay() <= base);
    }

    #[test]
    fn backoff_jitter_actually_varies() {
        let mut b = Backoff::with_params(Duration::from_secs(1), Duration::from_secs(300));
        // Burn to a wide ceiling, then sample.
        for _ in 0..9 {
            b.next_delay();
        }
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..16 {
            b.attempt = 9; // hold the ceiling at 512s->clamped 300s
            seen.insert(b.next_delay().as_millis());
        }
        assert!(seen.len() > 1, "full jitter must not be constant: {seen:?}");
    }
}
