//! ext-sse (REV-003) — the live status stream endpoint (build item
//! C8).
//!
//! Normative source: spec/reeve/04-status-stream.md §6:
//! - §6.1: `GET /api/reeve/v1/events`, `text/event-stream`, never
//!   unauthenticated — mounted behind the D1 human-auth middleware
//!   (router.rs), viewer+ enforced here (authorization matches the
//!   corresponding REST reads; v1 single-tier has no per-device read
//!   scoping, so viewer+ IS the read granularity). Optional `types`
//!   query filter; unknown names ignored.
//! - §6.2: per-stream monotonic `id:`; `Last-Event-ID` replay from
//!   the hub's bounded in-memory buffer (events.rs); when replay is
//!   impossible a `reset` event is sent FIRST; `:ka` comments every
//!   15 s (≤ 30 s required); at-most-once — a lagged consumer gets
//!   `reset` and refetches (drop + advise re-sync).
//! - §6.3: payloads are the typed `reeve_types::reeve::events`
//!   shapes. Producers wired in C8: status ingest, channel presence,
//!   terminal lifecycle, secrets rotation, and the durability sampler
//!   below. `rollout` (C9) and `health-state` producers emit through
//!   the same [`EventHub::emit`] seam when they land.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse as _, Response};
use device_api::{Identity, Role};
use futures_util::StreamExt as _;
use reeve_types::reeve::events::{
    DurabilityLagEvent, ResetEvent, SseEvent, VerifyRestoreEvent, VerifyRestoreOutcome,
    event_type,
};
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::durability::{Durability, DurabilityStatus};
use crate::events::{EventHub, Stamped};
use crate::state::AppState;

/// `durability-lag` threshold (§6.3: "changeset upload lag
/// crosses/clears a threshold — ops dashboard signal").
pub const LAG_THRESHOLD_SECS: u64 = 30;

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// §6.1: comma-separated event-type filter; unknown names ignored.
    pub types: Option<String>,
}

/// Type filter from the `types` query parameter. `None` = no filter.
fn parse_types(raw: Option<&str>) -> Option<HashSet<String>> {
    let raw = raw?;
    Some(
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn passes(filter: &Option<HashSet<String>>, event: &SseEvent) -> bool {
    match filter {
        None => true,
        Some(set) => set.contains(event.event_type()),
    }
}

fn wire_event(stamped: &Stamped) -> Event {
    Event::default()
        .id(stamped.id.to_string())
        .event(stamped.event.event_type())
        .data(stamped.event.data_json().unwrap_or_else(|_| "{}".into()))
}

/// The un-stamped `reset` (§6.2): carries no `id:` so the client's
/// Last-Event-ID tracking restarts from the next real event.
fn reset_event() -> Event {
    let payload = SseEvent::Reset(ResetEvent {
        ts: EventHub::now_ts(),
    });
    Event::default()
        .event(event_type::RESET)
        .data(payload.data_json().unwrap_or_else(|_| "{}".into()))
}

/// GET /api/reeve/v1/events (§6.1) — viewer+.
///
/// Event payload schemas (the rev-003/1 table, §6.3) are registered
/// as OpenAPI components by `openapi.rs` (D10: the UI's invalidation
/// handlers consume generated event types); the stream itself is SSE
/// (`event:` = type name, `data:` = the JSON payload, `id:` = the
/// per-stream monotonic id).
#[utoipa::path(
    get,
    path = "/api/reeve/v1/events",
    tag = "events",
    params(
        ("types" = Option<String>, Query, description = "Comma-separated event-type filter (§6.1); unknown names ignored. Types: reset, device-presence, deployment-status, terminal-session, health-state, verify-restore, durability-lag, rollout, secret-rotation, federation-sync"),
        ("Last-Event-ID" = Option<u64>, Header, description = "Resume after this event id (§6.2); when replay is impossible a `reset` event is sent first"),
    ),
    responses(
        (status = 200, description = "Server-Sent Events stream of cache-invalidation hints (§6.2: droppable, at-most-once)", content_type = "text/event-stream", body = String),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
    ),
)]
pub async fn events_route(
    State(state): State<AppState>,
    identity: Identity,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let filter = parse_types(query.types.as_deref());
    let last_event_id: Option<u64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());

    let sub = state.events.subscribe(last_event_id);

    // Head of the stream: `reset` first when replay is impossible
    // (§6.2 MUST), then any replayable buffered events.
    let mut head: Vec<Event> = Vec::new();
    if sub.needs_reset {
        head.push(reset_event());
    }
    for stamped in &sub.replay {
        if passes(&filter, &stamped.event) {
            head.push(wire_event(stamped));
        }
    }

    let live = futures_util::stream::unfold(
        (sub.rx, filter),
        |(mut rx, filter)| async move {
            loop {
                match rx.recv().await {
                    Ok(stamped) => {
                        if passes(&filter, &stamped.event) {
                            return Some((wire_event(&stamped), (rx, filter)));
                        }
                        // filtered out — keep waiting
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // §6.2 at-most-once: this consumer fell behind;
                        // drop what was missed and advise a full
                        // re-sync. Resubscription is implicit — the
                        // receiver skips to the newest events.
                        tracing::debug!(missed = n, "sse consumer lagged; sending reset");
                        return Some((reset_event(), (rx, filter)));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    let stream = futures_util::stream::iter(head)
        .chain(live)
        .map(Ok::<Event, Infallible>);

    Sse::new(stream)
        .keep_alive(
            // §6.2: an SSE comment at least every 30 s so proxies do
            // not idle-close the stream.
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ka"),
        )
        .into_response()
}

// ---------------------------------------------------------------
// Durability sampling (§6.3 durability-lag / verify-restore rows)
// ---------------------------------------------------------------

/// Transition memory between samples.
#[derive(Debug, Default)]
pub struct SamplerState {
    lagging: bool,
    last_verify_at: Option<i64>,
}

impl SamplerState {
    /// Seed from the current status so boot does not re-announce a
    /// verify run that finished before this process existed.
    pub fn seeded(status: &DurabilityStatus) -> Self {
        SamplerState {
            lagging: false,
            last_verify_at: status.last_verify.as_ref().map(|v| v.finished_at),
        }
    }
}

/// One sampling step, pure: compare `status` against the previous
/// sample and produce the transition events (§6.3: `durability-lag`
/// fires when the lag CROSSES or CLEARS the threshold, not on every
/// sample; `verify-restore` fires when a run completes).
pub fn sample_events(
    prev: &mut SamplerState,
    status: &DurabilityStatus,
    now_secs: i64,
    threshold_secs: u64,
) -> Vec<SseEvent> {
    let mut out = Vec::new();

    // Changeset upload lag (§9.3 shipping is keyed by generation +
    // monotonic seq): pending changesets that have not shipped since
    // `last_changeset_at` are the lag. No pending uploads = no lag.
    let lag_seconds: u64 = if status.pending_changesets > 0 {
        match status.last_changeset_at {
            Some(t) => (now_secs - t).max(0) as u64,
            // Pending but never shipped anything: unknown age — treat
            // as lagging (fail loud on the ops dashboard).
            None => threshold_secs + 1,
        }
    } else {
        0
    };
    let lagging = lag_seconds > threshold_secs;
    if lagging != prev.lagging {
        prev.lagging = lagging;
        out.push(SseEvent::DurabilityLag(DurabilityLagEvent {
            ts: EventHub::now_ts(),
            generation: status.generation.clone().unwrap_or_default(),
            last_seq: status.last_changeset_seq.unwrap_or(0),
            lag_seconds,
        }));
    }

    // verify-restore completion (§9.4 surfacing).
    if let Some(v) = &status.last_verify
        && prev.last_verify_at != Some(v.finished_at)
    {
        prev.last_verify_at = Some(v.finished_at);
        let snapshot_ts = v.generation.clone().unwrap_or_else(|| {
            chrono::DateTime::from_timestamp(v.finished_at, 0)
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_default()
        });
        out.push(SseEvent::VerifyRestore(VerifyRestoreEvent {
            ts: EventHub::now_ts(),
            outcome: if v.outcome == "ok" {
                VerifyRestoreOutcome::Ok
            } else {
                VerifyRestoreOutcome::Failed
            },
            snapshot_ts,
            detail: v.detail.clone(),
        }));
    }

    out
}

/// The sampling task: poll `durability.status()` every `every` and
/// emit transitions. No-op (not spawned) on the none tier.
pub fn spawn_durability_sampler(
    durability: Arc<dyn Durability>,
    hub: EventHub,
    every: Duration,
) {
    if durability.tier() == "none" {
        return;
    }
    tokio::spawn(async move {
        let mut prev = SamplerState::seeded(&durability.status());
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let status = durability.status();
            for event in
                sample_events(&mut prev, &status, crate::db::now_secs(), LAG_THRESHOLD_SECS)
            {
                hub.emit(event);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durability::VerifySummary;

    fn status() -> DurabilityStatus {
        DurabilityStatus {
            tier: "changeset".into(),
            degraded: false,
            last_error: None,
            generation: Some("2026-07-10T00:00:00Z-7".into()),
            last_snapshot_at: Some(1000),
            snapshot_age_secs: Some(10),
            last_changeset_seq: Some(42),
            last_changeset_at: Some(1000),
            pending_changesets: 0,
            last_verify: None,
        }
    }

    #[test]
    fn lag_fires_on_cross_and_clear_only() {
        let mut prev = SamplerState::default();
        let mut s = status();

        // No pending uploads: no lag, no event.
        assert!(sample_events(&mut prev, &s, 2000, 30).is_empty());

        // Pending + stale: crosses the threshold — one event.
        s.pending_changesets = 3;
        let events = sample_events(&mut prev, &s, 1100, 30);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SseEvent::DurabilityLag(e) => {
                assert_eq!(e.lag_seconds, 100);
                assert_eq!(e.last_seq, 42);
                assert_eq!(e.generation, "2026-07-10T00:00:00Z-7");
            }
            other => panic!("expected durability-lag, got {other:?}"),
        }
        // Still lagging: NOT re-emitted (crosses/clears only, §6.3).
        assert!(sample_events(&mut prev, &s, 1200, 30).is_empty());

        // Uploads drain: clears — one event with lag 0.
        s.pending_changesets = 0;
        let events = sample_events(&mut prev, &s, 1300, 30);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SseEvent::DurabilityLag(e) => assert_eq!(e.lag_seconds, 0),
            other => panic!("expected durability-lag, got {other:?}"),
        }
        assert!(sample_events(&mut prev, &s, 1400, 30).is_empty());
    }

    #[test]
    fn verify_restore_fires_once_per_run() {
        let mut prev = SamplerState::default();
        let mut s = status();
        s.last_verify = Some(VerifySummary {
            finished_at: 1500,
            outcome: "ok".into(),
            generation: Some("g-1".into()),
            last_seq: Some(7),
            detail: None,
        });
        let events = sample_events(&mut prev, &s, 1501, 30);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SseEvent::VerifyRestore(e) => {
                assert_eq!(e.outcome, VerifyRestoreOutcome::Ok);
                assert_eq!(e.snapshot_ts, "g-1");
            }
            other => panic!("expected verify-restore, got {other:?}"),
        }
        // Same run: silent.
        assert!(sample_events(&mut prev, &s, 1502, 30).is_empty());

        // A failed run later: fires again with the failure.
        s.last_verify = Some(VerifySummary {
            finished_at: 1600,
            outcome: "failed".into(),
            generation: None,
            last_seq: None,
            detail: Some("chain hole".into()),
        });
        let events = sample_events(&mut prev, &s, 1601, 30);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SseEvent::VerifyRestore(e) => {
                assert_eq!(e.outcome, VerifyRestoreOutcome::Failed);
                assert_eq!(e.detail.as_deref(), Some("chain hole"));
            }
            other => panic!("expected verify-restore, got {other:?}"),
        }
    }

    #[test]
    fn seeded_state_skips_stale_verify() {
        let mut s = status();
        s.last_verify = Some(VerifySummary {
            finished_at: 900,
            outcome: "ok".into(),
            generation: None,
            last_seq: None,
            detail: None,
        });
        let mut prev = SamplerState::seeded(&s);
        assert!(
            sample_events(&mut prev, &s, 1000, 30).is_empty(),
            "boot must not re-announce a pre-boot verify run"
        );
    }

    #[test]
    fn types_filter_grammar() {
        assert_eq!(parse_types(None), None);
        let set = parse_types(Some("device-presence, rollout,,junk")).unwrap();
        assert!(set.contains("device-presence") && set.contains("rollout"));
        // Unknown names are carried but match nothing — ignored (§6.1).
        assert!(set.contains("junk"));
    }
}
