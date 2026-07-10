//! Per-device channel registry (C8) — the server-side shared state
//! behind the REV-001 persistent agent channel
//! (spec/reeve/02-channel.md §4).
//!
//! CORE module, deliberately: presence (presence.rs, §4.3
//! "presence-as-fact") and the render pipeline's nudge hook
//! (render.rs, §4.4) consult this registry from core code. The
//! websocket endpoint that POPULATES it is `ext/channel.rs`, behind
//! the `ext-channel` feature — a core (--no-default-features) build
//! carries an always-empty registry, so presence degrades to polling
//! recency and nudges are no-ops, exactly the §3.2 degradation for a
//! server without the extension.
//!
//! One channel per device (§4.1): [`Channels::register`] atomically
//! replaces any previous handle — the old connection task is told to
//! close via its `replaced` [`Notify`] (crash-only, no draining) and
//! its later [`Channels::deregister`] is a no-op because the conn_id
//! no longer matches, so presence never flaps on replace.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use reeve_types::reeve::channel::{ControlFrame, NUDGE_SCOPE_DESIRED_STATE};
use tokio::sync::{Notify, mpsc, oneshot};

/// Outbound-queue depth per device connection. Nudges use `try_send`
/// (best-effort by spec, §4.4); sub-channel data uses `send` —
/// backpressure toward the producer, bounded by the socket.
const OUTBOUND_CAPACITY: usize = 64;

/// What extensions hand the connection task to send toward one
/// device's socket.
pub enum Outbound {
    /// One control frame, verbatim (§4.2).
    Control(ControlFrame),
    /// One data frame for an open sub-channel (§4.2 binary framing —
    /// the connection task adds the u32 BE id prefix).
    Data { id: u32, payload: Vec<u8> },
    /// Request a server-opened (even id, §4.2) sub-channel; the
    /// connection task allocates the id, sends `open`, and answers on
    /// `reply` when the agent `accept`s or `reject`s.
    OpenSub {
        purpose: String,
        meta: Option<serde_json::Value>,
        reply: oneshot::Sender<Result<SubChannel, String>>,
    },
    /// Close a sub-channel from the server side (§4.2 `close`).
    CloseSub { id: u32, reason: Option<String> },
}

/// A live server-opened sub-channel, as handed back on `accept`.
pub struct SubChannel {
    pub id: u32,
    /// Incoming payloads (id prefix already stripped) and the close
    /// notification. Channel teardown implicitly closes every
    /// sub-channel (§4.2) — consumers always see [`SubEvent::Closed`]
    /// (or the sender dropping) as a normal event, not corruption.
    pub rx: mpsc::Receiver<SubEvent>,
}

/// One event on a sub-channel's receive side.
#[derive(Debug, PartialEq, Eq)]
pub enum SubEvent {
    Data(Vec<u8>),
    Closed,
}

struct Handle {
    conn_id: u64,
    /// Unix seconds the channel opened — presence "online since"
    /// (§4.3).
    since: i64,
    tx: mpsc::Sender<Outbound>,
    replaced: Arc<Notify>,
}

/// What [`Channels::register`] hands a new connection task.
pub struct Registration {
    /// This connection's identity for [`Channels::deregister`].
    pub conn_id: u64,
    /// The outbound queue this connection drains toward its socket.
    pub rx: mpsc::Receiver<Outbound>,
    /// Notified when a newer channel for the same device replaced
    /// this one (§4.1) — close the socket immediately, no draining.
    pub replaced: Arc<Notify>,
    /// True when this register replaced a live channel (no presence
    /// transition — the device stayed online).
    pub replaced_existing: bool,
}

/// The registry: device_id -> live channel handle. Cloneable, shared
/// through [`crate::state::AppState`].
#[derive(Clone, Default)]
pub struct Channels {
    inner: Arc<Mutex<HashMap<String, Handle>>>,
    next_conn: Arc<AtomicU64>,
}

impl Channels {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the authenticated channel for `device_id`, atomically
    /// replacing any previous one (§4.1: old socket closed, crash-only,
    /// no draining — the server MUST tolerate reconnect storms, §4.5).
    pub fn register(&self, device_id: &str) -> Registration {
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let replaced = Arc::new(Notify::new());
        let handle = Handle {
            conn_id,
            since: crate::db::now_secs(),
            tx,
            replaced: replaced.clone(),
        };
        let old = {
            let mut map = self.inner.lock().expect("channels mutex poisoned");
            map.insert(device_id.to_string(), handle)
        };
        let replaced_existing = old.is_some();
        if let Some(old) = old {
            // notify_one stores a permit: the old connection task sees
            // the replacement even if it is not parked on notified()
            // at this instant (it re-arms per select iteration).
            old.replaced.notify_one();
        }
        Registration {
            conn_id,
            rx,
            replaced,
            replaced_existing,
        }
    }

    /// Remove this connection's handle — only if it is still the
    /// current one (a replaced connection must not evict its
    /// replacement). Returns `true` when the device just went
    /// offline (presence transition, §4.3).
    pub fn deregister(&self, device_id: &str, conn_id: u64) -> bool {
        let mut map = self.inner.lock().expect("channels mutex poisoned");
        match map.get(device_id) {
            Some(h) if h.conn_id == conn_id => {
                map.remove(device_id);
                true
            }
            _ => false,
        }
    }

    /// Presence-as-fact (§4.3): `Some(open-since unix secs)` iff this
    /// device's channel is open right now.
    pub fn online_since(&self, device_id: &str) -> Option<i64> {
        self.inner
            .lock()
            .expect("channels mutex poisoned")
            .get(device_id)
            .map(|h| h.since)
    }

    /// The outbound sender toward a device's socket, for extensions
    /// opening sub-channels (ext/terminal.rs). `None` = offline.
    pub fn sender(&self, device_id: &str) -> Option<mpsc::Sender<Outbound>> {
        self.inner
            .lock()
            .expect("channels mutex poisoned")
            .get(device_id)
            .map(|h| h.tx.clone())
    }

    /// Best-effort `nudge` scope `desired-state` (§4.4): no retry, no
    /// queue for offline devices, no ack tracking — a full queue or an
    /// absent channel drops the nudge, costing one poll interval of
    /// latency and nothing else.
    pub fn nudge_desired_state(&self, device_id: &str) {
        let tx = {
            let map = self.inner.lock().expect("channels mutex poisoned");
            match map.get(device_id) {
                Some(h) => h.tx.clone(),
                None => return,
            }
        };
        let _ = tx.try_send(Outbound::Control(ControlFrame::Nudge {
            scope: NUDGE_SCOPE_DESIRED_STATE.to_string(),
            hint: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_replace_deregister_lifecycle() {
        let ch = Channels::new();
        assert_eq!(ch.online_since("dev-1"), None);

        let first = ch.register("dev-1");
        assert!(!first.replaced_existing);
        assert!(ch.online_since("dev-1").is_some());

        // New channel atomically replaces the old (§4.1): the old task
        // is notified; presence stays online.
        let second = ch.register("dev-1");
        assert!(second.replaced_existing);
        tokio::time::timeout(std::time::Duration::from_secs(1), first.replaced.notified())
            .await
            .expect("old connection must be told to close");
        assert!(ch.online_since("dev-1").is_some());

        // The replaced connection's deregister is a no-op (§4.3: no
        // presence flap on replace).
        assert!(!ch.deregister("dev-1", first.conn_id));
        assert!(ch.online_since("dev-1").is_some());

        // The current connection's deregister takes the device offline.
        assert!(ch.deregister("dev-1", second.conn_id));
        assert_eq!(ch.online_since("dev-1"), None);
    }

    #[tokio::test]
    async fn nudge_is_best_effort() {
        let ch = Channels::new();
        // Absent channel: dropped silently (§4.4).
        ch.nudge_desired_state("dev-1");

        let mut reg = ch.register("dev-1");
        ch.nudge_desired_state("dev-1");
        match reg.rx.recv().await {
            Some(Outbound::Control(ControlFrame::Nudge { scope, .. })) => {
                assert_eq!(scope, NUDGE_SCOPE_DESIRED_STATE);
            }
            other => panic!("expected nudge, got {:?}", other.is_some()),
        }

        // A full queue drops nudges rather than blocking (§4.4).
        for _ in 0..(OUTBOUND_CAPACITY + 10) {
            ch.nudge_desired_state("dev-1");
        }
    }
}
