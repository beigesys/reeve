//! ext-channel (REV-001) — the server side of the persistent agent
//! channel (build item C8).
//!
//! Normative source: spec/reeve/02-channel.md §4. Load-bearing rules
//! implemented here:
//! - §4.1: endpoint `GET /api/reeve/v1/channel` (websocket upgrade),
//!   authenticated with the SAME device credential as the device API
//!   — the route is mounted behind `device_api::device_auth`
//!   (router.rs), so unknown/unauthenticated clients are rejected
//!   BEFORE upgrade. One channel per device: a new authenticated
//!   channel atomically replaces the old (channels.rs; old socket
//!   closed, crash-only, no draining). Reconnect storms are tolerated
//!   by construction — replace is one map insert.
//! - §4.2: control frames (text JSON, unknown `type` ignored) and
//!   data frames (binary, u32 BE sub-channel id). Server-allocated
//!   sub-channel ids are EVEN. `open` from the agent for an
//!   unsupported purpose => `reject`, never teardown. Data for a
//!   non-open id is discarded silently. `hello` is sent once at open,
//!   both directions.
//! - §4.3: presence-as-fact — this connection IS the presence signal
//!   (channels.rs registry, presence.rs consumes). Ping when idle
//!   ≥ 30 s; a missing pong within 10 s is a dead channel: close the
//!   socket, emit the `device-presence` event.
//! - §4.4: nudges are sent by the render pipeline through
//!   [`crate::channels::Channels::nudge_desired_state`] — best
//!   effort, no retry/queue/ack.
//! - §4.7: resource limits — max frame size 1 MiB, per-device
//!   sub-channel cap 16.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use device_api::DeviceIdentity;
use reeve_types::reeve::channel::{
    CHANNEL_PROTOCOL, ControlFrame, KEEPALIVE_IDLE_SECS, MAX_FRAME_BYTES, MAX_SUB_CHANNELS,
    PONG_TIMEOUT_SECS, decode_data_frame, encode_data_frame,
};
use reeve_types::reeve::events::{DevicePresenceEvent, PresenceState, SseEvent};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::channels::{Outbound, SubChannel, SubEvent};
use crate::events::EventHub;
use crate::state::AppState;

/// Sub-channel purposes THIS server can open toward an agent —
/// advertised in our `hello.extensions` (§4.2).
fn server_purposes() -> Vec<String> {
    if cfg!(feature = "ext-terminal") {
        vec![reeve_types::reeve::channel::PURPOSE_TERMINAL.to_string()]
    } else {
        Vec::new()
    }
}

/// GET /api/reeve/v1/channel (§4.1). `device_auth` already resolved
/// the bearer token — an unauthenticated upgrade never reaches here.
pub async fn channel_route(
    State(state): State<AppState>,
    DeviceIdentity(device_id): DeviceIdentity,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(MAX_FRAME_BYTES) // §4.7 resource limits
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| connection(state, device_id, socket))
}

/// Emit a `device-presence` transition (§4.3; 04-status-stream §6.3).
fn emit_presence(events: &EventHub, device_id: &str, online: bool) {
    events.emit(SseEvent::DevicePresence(DevicePresenceEvent {
        ts: EventHub::now_ts(),
        device_id: device_id.to_string(),
        state: if online {
            PresenceState::Online
        } else {
            PresenceState::Offline
        },
        since: EventHub::now_ts(),
    }));
}

/// One channel connection: register (replacing any previous channel),
/// hello, serve until the socket dies / keepalive fails / a newer
/// channel replaces us, then deregister and emit presence.
async fn connection(state: AppState, device_id: String, socket: WebSocket) {
    let reg = state.channels.register(&device_id);
    if reg.replaced_existing {
        info!(device = %device_id, "channel replaced (one channel per device, §4.1)");
    } else {
        info!(device = %device_id, "channel open");
        emit_presence(&state.events, &device_id, true);
    }

    let conn_id = reg.conn_id;
    let result = serve(socket, reg.rx, reg.replaced).await;
    match &result {
        Ok(reason) => debug!(device = %device_id, reason, "channel closed"),
        Err(e) => debug!(device = %device_id, error = %e, "channel error"),
    }

    // §4.3: a replaced connection's deregister is a no-op — presence
    // stays online under the new channel, no offline event.
    if state.channels.deregister(&device_id, conn_id) {
        info!(device = %device_id, "channel down (link down, never \"device dead\" — §4.3)");
        emit_presence(&state.events, &device_id, false);
    }
}

fn control_msg(frame: &ControlFrame) -> Message {
    Message::Text(
        serde_json::to_string(frame)
            .expect("control frames always serialize")
            .into(),
    )
}

/// A live server-opened sub-channel's local bookkeeping.
struct Sub {
    tx: mpsc::Sender<SubEvent>,
}

/// The per-connection actor: hello, then select over the socket, the
/// outbound queue (nudges, sub-channel data/opens from extensions),
/// the replaced signal, and the keepalive timer. Returns the close
/// reason.
async fn serve(
    mut socket: WebSocket,
    mut out_rx: mpsc::Receiver<Outbound>,
    replaced: std::sync::Arc<tokio::sync::Notify>,
) -> anyhow::Result<String> {
    // hello, once at open (§4.2): protocol + purposes we may open.
    socket
        .send(control_msg(&ControlFrame::Hello {
            protocol: CHANNEL_PROTOCOL.to_string(),
            extensions: server_purposes(),
        }))
        .await?;

    // Server-opened sub-channels (even ids, §4.2).
    let mut subs: HashMap<u32, Sub> = HashMap::new();
    let mut pending: HashMap<u32, oneshot::Sender<Result<SubChannel, String>>> = HashMap::new();
    let mut next_even_id: u32 = 2;

    let idle = Duration::from_secs(KEEPALIVE_IDLE_SECS);
    let pong_timeout = Duration::from_secs(PONG_TIMEOUT_SECS);
    let mut last_rx = Instant::now();
    // Some(nonce, deadline) while a ping awaits its pong (§4.3).
    let mut pending_pong: Option<(String, Instant)> = None;
    let mut nonce_counter: u64 = 0;

    let result: anyhow::Result<String> = loop {
        let deadline = match &pending_pong {
            Some((_, d)) => *d,
            None => last_rx + idle,
        };
        tokio::select! {
            msg = socket.recv() => {
                let msg = match msg {
                    None => break Ok("peer hung up".into()),
                    Some(Err(e)) => break Err(e.into()),
                    Some(Ok(m)) => m,
                };
                last_rx = Instant::now();
                match msg {
                    Message::Text(text) => {
                        // Unknown `type` deserializes to Unknown and is
                        // ignored; malformed JSON is ignored the same
                        // way (tolerant reader, 01-framework §3.4).
                        let frame = match serde_json::from_str::<ControlFrame>(text.as_str()) {
                            Ok(f) => f,
                            Err(e) => {
                                debug!(error = %e, "unparseable control frame ignored");
                                continue;
                            }
                        };
                        match frame {
                            ControlFrame::Hello { protocol, extensions } => {
                                debug!(%protocol, ?extensions, "agent hello");
                            }
                            ControlFrame::Ping { nonce } => {
                                socket.send(control_msg(&ControlFrame::Pong { nonce })).await?;
                            }
                            ControlFrame::Pong { nonce } => {
                                match &pending_pong {
                                    Some((expect, _)) if *expect == nonce => pending_pong = None,
                                    _ => debug!(%nonce, "unsolicited pong ignored"),
                                }
                            }
                            ControlFrame::Open { id, purpose, .. } => {
                                // No agent-initiated purposes are
                                // registered server-side in rev-001/1:
                                // reject, never teardown (§4.2).
                                socket.send(control_msg(&ControlFrame::Reject {
                                    id,
                                    reason: format!("unsupported purpose {purpose:?}"),
                                })).await?;
                            }
                            ControlFrame::Accept { id } => {
                                if let Some(reply) = pending.remove(&id) {
                                    let (tx, rx) = mpsc::channel(64);
                                    subs.insert(id, Sub { tx });
                                    let _ = reply.send(Ok(SubChannel { id, rx }));
                                } else {
                                    debug!(id, "accept with no pending open; ignored");
                                }
                            }
                            ControlFrame::Reject { id, reason } => {
                                if let Some(reply) = pending.remove(&id) {
                                    let _ = reply.send(Err(reason));
                                } else {
                                    debug!(id, "reject with no pending open; ignored");
                                }
                            }
                            ControlFrame::Close { id, .. } => {
                                if let Some(sub) = subs.remove(&id) {
                                    let _ = sub.tx.send(SubEvent::Closed).await;
                                }
                                // Unknown id: frames race close — silent (§4.2).
                            }
                            ControlFrame::Nudge { .. } => {
                                debug!("agent-sent nudge ignored (server has no poll loop)");
                            }
                            ControlFrame::Unknown => {} // §4.2: MUST ignore
                        }
                    }
                    Message::Binary(data) => {
                        // Route by sub-channel id; anything not
                        // accepted-and-open is discarded silently
                        // (§4.2 — frames race close). The consumer
                        // send is awaited: backpressure toward the
                        // socket, bounded by TCP, resolved by the
                        // bridge's independent pump tasks.
                        if let Some((id, payload)) = decode_data_frame(&data)
                            && let Some(sub) = subs.get(&id)
                            && sub.tx.send(SubEvent::Data(payload.to_vec())).await.is_err()
                        {
                            // Consumer gone: close our side.
                            subs.remove(&id);
                            socket.send(control_msg(&ControlFrame::Close {
                                id,
                                reason: Some("consumer gone".into()),
                            })).await?;
                        }
                    }
                    Message::Close(_) => break Ok("peer closed".into()),
                    // Transport ping/pong: axum/tungstenite answers
                    // pings itself; both refreshed last_rx above.
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            out = out_rx.recv() => {
                // The Channels handle is held by the registry until
                // deregister, so recv() only yields None after replace.
                let Some(out) = out else { break Ok("registry dropped".into()) };
                match out {
                    Outbound::Control(frame) => {
                        socket.send(control_msg(&frame)).await?;
                    }
                    Outbound::Data { id, payload } => {
                        // Relay for an id we no longer track races
                        // close — the agent discards it (§4.2).
                        socket.send(Message::Binary(
                            encode_data_frame(id, &payload).into(),
                        )).await?;
                    }
                    Outbound::OpenSub { purpose, meta, reply } => {
                        if subs.len() + pending.len() >= MAX_SUB_CHANNELS {
                            let _ = reply.send(Err(format!(
                                "sub-channel cap {MAX_SUB_CHANNELS} reached"
                            )));
                            continue;
                        }
                        let id = next_even_id;
                        next_even_id = next_even_id.wrapping_add(2).max(2);
                        pending.insert(id, reply);
                        socket.send(control_msg(&ControlFrame::Open {
                            id,
                            purpose,
                            meta,
                        })).await?;
                    }
                    Outbound::CloseSub { id, reason } => {
                        if let Some(sub) = subs.remove(&id) {
                            let _ = sub.tx.send(SubEvent::Closed).await;
                        }
                        socket.send(control_msg(&ControlFrame::Close { id, reason })).await?;
                    }
                }
            }
            _ = replaced.notified() => {
                // §4.1: atomically replaced — close now, no draining.
                break Ok("replaced by a newer channel".into());
            }
            _ = tokio::time::sleep_until(deadline) => {
                match pending_pong.take() {
                    Some((nonce, _)) => {
                        // §4.3: missing pong within the timeout is a
                        // dead channel.
                        break Err(anyhow::anyhow!(
                            "keepalive: no pong for nonce {nonce} within {pong_timeout:?}"
                        ));
                    }
                    None => {
                        nonce_counter += 1;
                        let nonce = format!("srv-{nonce_counter:x}");
                        socket.send(control_msg(&ControlFrame::Ping {
                            nonce: nonce.clone(),
                        })).await?;
                        pending_pong = Some((nonce, Instant::now() + pong_timeout));
                    }
                }
            }
        }
    };

    // Channel teardown implicitly closes all sub-channels — a normal
    // event, not corruption (§4.2). Pending opens fail the same way.
    for (_, sub) in subs.drain() {
        let _ = sub.tx.send(SubEvent::Closed).await;
    }
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err("channel closed".into()));
    }
    if let Err(e) = &result {
        warn!(error = %e, "channel serve ended with error");
    }
    result
}
