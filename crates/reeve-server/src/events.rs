//! The in-process event hub (C8) — the fan-out point behind the
//! REV-003 live status stream (spec/reeve/04-status-stream.md §6).
//!
//! CORE module, deliberately: producers live in core (status ingest,
//! render pipeline) and in extensions (channel presence, terminal
//! lifecycle, secrets rotation), so the hub itself must not be
//! feature-gated — with `ext-sse` compiled out the hub is a broadcast
//! channel nobody subscribes to and every emit is a no-op-priced
//! `send` to zero receivers. The HTTP endpoint (the actual REV-003
//! surface) is `ext/sse.rs`, behind the `ext-sse` feature.
//!
//! Delivery semantics implemented here (§6.2):
//! - every event carries a monotonically increasing per-stream id,
//!   stamped at emit;
//! - a bounded in-memory replay buffer (last [`REPLAY_MAX_EVENTS`]
//!   events or [`REPLAY_MAX_AGE`], whichever smaller) serves
//!   `Last-Event-ID` reconnects — best-effort, RAM only, empty after
//!   restart, which is correct because clients refetch on reconnect
//!   (Law 3: no persisted event log, nothing to flush at shutdown);
//! - delivery is at-most-once; producers MUST NOT rely on a UI
//!   having seen any event — [`EventHub::emit`] never blocks and
//!   never errors.
//!
//! Producer seams wired in C8: status ingest (`deployment-status`,
//! ingest.rs), channel presence (`device-presence`, ext/channel.rs),
//! terminal lifecycle (`terminal-session`, ext/terminal.rs), secrets
//! rotation (`secret-rotation`, ext/secrets.rs), durability sampling
//! (`durability-lag` / `verify-restore`, ext/sse.rs). `rollout`
//! (09-rollouts §11.6, C9) and `health-state` (05-health-journal
//! §7.4) producers land later; their seam is exactly
//! [`EventHub::emit`] with the already-typed payloads in
//! `reeve_types::reeve::events`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use reeve_types::reeve::events::SseEvent;
use tokio::sync::broadcast;

/// Replay buffer bound: event count (§6.2 RECOMMENDED 256).
pub const REPLAY_MAX_EVENTS: usize = 256;
/// Replay buffer bound: age (§6.2 RECOMMENDED 60 s).
pub const REPLAY_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);
/// Broadcast capacity per subscriber before it lags (§6.2: a lagged
/// consumer is sent `reset` and refetches — see ext/sse.rs).
const BROADCAST_CAPACITY: usize = 1024;

/// One emitted event: the §6.2 per-stream `id:` plus the typed
/// payload.
#[derive(Debug, Clone)]
pub struct Stamped {
    pub id: u64,
    pub event: SseEvent,
}

struct HubInner {
    /// Next id to stamp; ids start at 1 each boot (in-memory only —
    /// a restart resets the stream, and clients refetch on `reset`).
    next_id: u64,
    buffer: VecDeque<(Instant, Stamped)>,
}

/// Cloneable fan-out handle threaded through [`crate::state::AppState`].
#[derive(Clone)]
pub struct EventHub {
    inner: Arc<Mutex<HubInner>>,
    tx: broadcast::Sender<Stamped>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`EventHub::subscribe`] hands an SSE connection.
pub struct Subscription {
    /// Buffered events after the client's `Last-Event-ID`, oldest
    /// first. Empty when the client sent no id (fresh connect).
    pub replay: Vec<Stamped>,
    /// §6.2: the server cannot replay from the client's
    /// `Last-Event-ID` — a `reset` event MUST be sent first.
    pub needs_reset: bool,
    /// Live events emitted after this subscription was taken. The
    /// snapshot and the receiver are created under one lock with
    /// emit, so the pair has no gap and no duplicate.
    pub rx: broadcast::Receiver<Stamped>,
}

impl EventHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        EventHub {
            inner: Arc::new(Mutex::new(HubInner {
                next_id: 1,
                buffer: VecDeque::new(),
            })),
            tx,
        }
    }

    /// Emit one event: stamp, buffer, broadcast. Never blocks, never
    /// errors — an event with no listeners is dropped, which is the
    /// §6.2 contract (droppable, at-most-once, cache-invalidation
    /// hints only).
    pub fn emit(&self, event: SseEvent) {
        let stamped = {
            let mut inner = self.inner.lock().expect("event hub mutex poisoned");
            let id = inner.next_id;
            inner.next_id += 1;
            let stamped = Stamped { id, event };
            let now = Instant::now();
            inner.buffer.push_back((now, stamped.clone()));
            while inner.buffer.len() > REPLAY_MAX_EVENTS {
                inner.buffer.pop_front();
            }
            while inner
                .buffer
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t) > REPLAY_MAX_AGE)
            {
                inner.buffer.pop_front();
            }
            // Broadcast under the lock so subscribe()'s snapshot+rx
            // pair is gapless (see Subscription::rx docs).
            let _ = self.tx.send(stamped.clone());
            stamped
        };
        let _ = stamped; // buffered + broadcast; nothing else to do
    }

    /// Subscribe, honoring an optional `Last-Event-ID` (§6.2).
    pub fn subscribe(&self, last_event_id: Option<u64>) -> Subscription {
        let inner = self.inner.lock().expect("event hub mutex poisoned");
        let rx = self.tx.subscribe();
        let last_emitted = inner.next_id - 1;
        let (replay, needs_reset) = match last_event_id {
            None => (Vec::new(), false),
            Some(id) if id > last_emitted => {
                // An id we never issued — a previous boot's stream.
                // Restart resets ids; the client must refetch (§6.2).
                (Vec::new(), true)
            }
            Some(id) => {
                let oldest_buffered = inner.buffer.front().map(|(_, s)| s.id);
                let contiguous = match oldest_buffered {
                    // Everything after `id` is still buffered?
                    Some(oldest) => id + 1 >= oldest,
                    // Empty buffer: fine iff the client is current.
                    None => id == last_emitted,
                };
                if contiguous {
                    (
                        inner
                            .buffer
                            .iter()
                            .filter(|(_, s)| s.id > id)
                            .map(|(_, s)| s.clone())
                            .collect(),
                        false,
                    )
                } else {
                    (Vec::new(), true)
                }
            }
        };
        Subscription {
            replay,
            needs_reset,
            rx,
        }
    }

    /// Current server time as the §6.3 payload `ts` (RFC 3339).
    pub fn now_ts() -> String {
        chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_types::reeve::events::{DevicePresenceEvent, PresenceState};

    fn ev(n: u64) -> SseEvent {
        SseEvent::DevicePresence(DevicePresenceEvent {
            ts: format!("t{n}"),
            device_id: format!("dev-{n}"),
            state: PresenceState::Online,
            since: "s".into(),
        })
    }

    #[tokio::test]
    async fn emit_stamps_monotonic_ids_and_broadcasts() {
        let hub = EventHub::new();
        let sub = hub.subscribe(None);
        assert!(sub.replay.is_empty());
        assert!(!sub.needs_reset);
        let mut rx = sub.rx;
        hub.emit(ev(1));
        hub.emit(ev(2));
        assert_eq!(rx.recv().await.unwrap().id, 1);
        assert_eq!(rx.recv().await.unwrap().id, 2);
    }

    #[test]
    fn replay_from_last_event_id() {
        let hub = EventHub::new();
        for n in 1..=5 {
            hub.emit(ev(n));
        }
        let sub = hub.subscribe(Some(3));
        assert!(!sub.needs_reset);
        assert_eq!(sub.replay.iter().map(|s| s.id).collect::<Vec<_>>(), [4, 5]);

        // Current client: nothing to replay, no reset.
        let sub = hub.subscribe(Some(5));
        assert!(!sub.needs_reset);
        assert!(sub.replay.is_empty());
    }

    #[test]
    fn unknown_or_evicted_ids_reset() {
        let hub = EventHub::new();
        hub.emit(ev(1));
        // Future id (previous boot): reset (§6.2 — restart resets).
        let sub = hub.subscribe(Some(99));
        assert!(sub.needs_reset && sub.replay.is_empty());

        // Evicted id: overflow the buffer, then ask from before it.
        for n in 0..(REPLAY_MAX_EVENTS as u64 + 10) {
            hub.emit(ev(n));
        }
        let sub = hub.subscribe(Some(1));
        assert!(sub.needs_reset, "id 1 fell out of the bounded buffer");
    }

    #[test]
    fn buffer_is_bounded() {
        let hub = EventHub::new();
        for n in 0..1000u64 {
            hub.emit(ev(n));
        }
        let inner = hub.inner.lock().unwrap();
        assert!(inner.buffer.len() <= REPLAY_MAX_EVENTS);
    }

    #[test]
    fn now_ts_is_rfc3339() {
        let ts = EventHub::now_ts();
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok(), "{ts}");
    }
}
