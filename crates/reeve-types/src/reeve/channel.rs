//! REV-001 Persistent Agent Channel — wire framing
//! (spec/reeve/02-channel.md §4.1–§4.2).
//!
//! The channel is one outbound websocket per device carrying two
//! frame kinds:
//! - **control frames**: websocket text, one JSON object with a
//!   `type` field. Unknown `type` values MUST be ignored
//!   (§4.2; spec/reeve/01-framework.md §3.4) — [`ControlFrame::Unknown`]
//!   absorbs them at deserialization time.
//! - **data frames**: websocket binary, first 4 bytes the
//!   sub-channel id (u32 big-endian), remainder opaque payload owned
//!   by the sub-channel's registering extension
//!   ([`encode_data_frame`] / [`decode_data_frame`]).

use serde::{Deserialize, Serialize};

/// Channel endpoint: `GET /api/reeve/v1/channel` with a standard
/// websocket upgrade, authenticated with the enrollment-issued
/// device credential (spec/reeve/02-channel.md §4.1).
pub const CHANNEL_PATH: &str = "/api/reeve/v1/channel";

/// The protocol id both sides state in `hello`
/// (spec/reeve/02-channel.md §4.2).
pub const CHANNEL_PROTOCOL: &str = "rev-001/1";

/// Registered sub-channel purpose: the remote terminal
/// (spec/reeve/02-channel.md §4.2; spec/reeve/03-terminal.md §5).
pub const PURPOSE_TERMINAL: &str = "rev-002/terminal";

/// `nudge` scope: a new manifestVersion is available — bundle and/or
/// secrets_version change; poll now (spec/reeve/02-channel.md §4.2,
/// §4.4).
pub const NUDGE_SCOPE_DESIRED_STATE: &str = "desired-state";

/// RECOMMENDED maximum websocket frame size
/// (spec/reeve/02-channel.md §4.7).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// RECOMMENDED per-device open sub-channel cap
/// (spec/reeve/02-channel.md §4.7).
pub const MAX_SUB_CHANNELS: usize = 16;

/// RECOMMENDED keepalive: ping when idle past this
/// (spec/reeve/02-channel.md §4.3).
pub const KEEPALIVE_IDLE_SECS: u64 = 30;

/// RECOMMENDED keepalive: a missing `pong` within this is a dead
/// channel (spec/reeve/02-channel.md §4.3).
pub const PONG_TIMEOUT_SECS: u64 = 10;

/// One control frame — websocket text, one JSON object
/// (spec/reeve/02-channel.md §4.2 table, rev-001/1).
///
/// The `nonce` on ping/pong and the `scope` on nudge are plain
/// strings: nonces are opaque match-me-back tokens, and unknown
/// scope values must be ignorable rather than a parse error
/// (spec/reeve/01-framework.md §3.4 tolerant reader).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ControlFrame {
    /// Both directions, once at open: protocol version + sub-channel
    /// purposes supported.
    Hello {
        /// `"rev-001/1"` ([`CHANNEL_PROTOCOL`]).
        protocol: String,
        /// Sub-channel purposes this side supports (e.g.
        /// `"rev-002/terminal"`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extensions: Vec<String>,
    },
    /// Server → agent: state worth polling for has changed (§4.4).
    Nudge {
        /// `"desired-state"` | `"config"`; unknown scopes ignored.
        scope: String,
        /// Opaque, OPTIONAL; MUST NOT carry secret material (§4.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<serde_json::Value>,
    },
    /// Either direction: request a sub-channel. Ids are allocated by
    /// the side sending `open`: agent odd, server even (§4.2).
    Open {
        id: u32,
        /// e.g. [`PURPOSE_TERMINAL`].
        purpose: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<serde_json::Value>,
    },
    /// Peer of `open`: sub-channel live.
    Accept { id: u32 },
    /// Peer of `open`: refused; id released. An unsupported `purpose`
    /// MUST be answered with `reject`, never by tearing down the
    /// channel (§4.2).
    Reject { id: u32, reason: String },
    /// Either direction: sub-channel closed; peers MUST discard
    /// in-flight data frames for `id` (§4.2).
    Close {
        id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Application-level liveness probe (§4.3).
    Ping { nonce: String },
    /// Answer to `ping`, echoing its `nonce` (§4.3).
    Pong { nonce: String },
    /// Any unrecognized `type` — MUST be ignored, never an error
    /// (§4.2). Never serialized.
    #[serde(other, skip_serializing)]
    Unknown,
}

/// True iff `id` is in the agent-allocated (odd) space (§4.2).
pub fn agent_allocated(id: u32) -> bool {
    id % 2 == 1
}

/// True iff `id` is in the server-allocated (even) space (§4.2).
pub fn server_allocated(id: u32) -> bool {
    id.is_multiple_of(2)
}

/// Encode a data frame: 4-byte u32 big-endian sub-channel id, then
/// the opaque payload (§4.2).
pub fn encode_data_frame(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&id.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Decode a data frame into `(sub_channel_id, payload)`. `None` for
/// frames shorter than the 4-byte id — the receiver discards those
/// silently, like any frame for a non-open id (§4.2).
pub fn decode_data_frame(frame: &[u8]) -> Option<(u32, &[u8])> {
    if frame.len() < 4 {
        return None;
    }
    let id = u32::from_be_bytes(frame[..4].try_into().ok()?);
    Some((id, &frame[4..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_frames_round_trip_with_spec_field_names() {
        let cases: Vec<(ControlFrame, serde_json::Value)> = vec![
            (
                ControlFrame::Hello {
                    protocol: CHANNEL_PROTOCOL.into(),
                    extensions: vec![PURPOSE_TERMINAL.into()],
                },
                serde_json::json!({"type": "hello", "protocol": "rev-001/1",
                                   "extensions": ["rev-002/terminal"]}),
            ),
            (
                ControlFrame::Nudge {
                    scope: NUDGE_SCOPE_DESIRED_STATE.into(),
                    hint: None,
                },
                serde_json::json!({"type": "nudge", "scope": "desired-state"}),
            ),
            (
                ControlFrame::Open {
                    id: 2,
                    purpose: PURPOSE_TERMINAL.into(),
                    meta: Some(serde_json::json!({"cols": 80})),
                },
                serde_json::json!({"type": "open", "id": 2,
                                   "purpose": "rev-002/terminal",
                                   "meta": {"cols": 80}}),
            ),
            (
                ControlFrame::Accept { id: 2 },
                serde_json::json!({"type": "accept", "id": 2}),
            ),
            (
                ControlFrame::Reject {
                    id: 4,
                    reason: "unsupported purpose".into(),
                },
                serde_json::json!({"type": "reject", "id": 4,
                                   "reason": "unsupported purpose"}),
            ),
            (
                ControlFrame::Close { id: 2, reason: None },
                serde_json::json!({"type": "close", "id": 2}),
            ),
            (
                ControlFrame::Ping { nonce: "n1".into() },
                serde_json::json!({"type": "ping", "nonce": "n1"}),
            ),
            (
                ControlFrame::Pong { nonce: "n1".into() },
                serde_json::json!({"type": "pong", "nonce": "n1"}),
            ),
        ];
        for (frame, wire) in cases {
            let ser = serde_json::to_value(&frame).unwrap();
            assert_eq!(ser, wire, "serialization of {frame:?}");
            let de: ControlFrame = serde_json::from_value(wire).unwrap();
            assert_eq!(de, frame);
        }
    }

    #[test]
    fn unknown_type_is_ignored_not_an_error() {
        // §4.2: unknown `type` values MUST be ignored.
        let de: ControlFrame =
            serde_json::from_str(r#"{"type": "frobnicate", "anything": [1, 2]}"#).unwrap();
        assert_eq!(de, ControlFrame::Unknown);
    }

    #[test]
    fn data_frame_round_trip() {
        let frame = encode_data_frame(0x0102_0304, b"payload");
        assert_eq!(&frame[..4], &[1, 2, 3, 4]);
        assert_eq!(decode_data_frame(&frame), Some((0x0102_0304u32, &b"payload"[..])));
        // Empty payload is legal.
        assert_eq!(decode_data_frame(&encode_data_frame(7, b"")), Some((7u32, &b""[..])));
        // Shorter than the id: discarded (None).
        assert_eq!(decode_data_frame(&[0, 0, 1]), None);
    }

    #[test]
    fn id_parity_spaces() {
        // §4.2: agent odd, server even — allocation never collides.
        assert!(agent_allocated(1) && agent_allocated(3));
        assert!(!agent_allocated(2));
        assert!(server_allocated(2) && server_allocated(0));
        assert!(!server_allocated(5));
    }
}
