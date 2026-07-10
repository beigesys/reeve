//! REV-002 Remote Terminal — wire and file shapes
//! (spec/reeve/03-terminal.md §5).
//!
//! Three surfaces live here:
//! - [`TerminalOpenMeta`] — the sub-channel `open.meta` bootstrap
//!   (§5.1: "sessionId, requested PTY size, TERM string" — nothing
//!   else; resize and control ride in-band).
//! - [`TerminalConfig`] — the enablement configuration item in the
//!   device's render bundle (§5.2: enablement is expressed ONLY in
//!   desired state; this file's shape is ours — the spec leaves the
//!   mechanism open, so the boring option is a rendered config file
//!   at [`TERMINAL_CONFIG_PATH`] that the agent reads from its
//!   swapped bundle and the server reads from its own render of the
//!   same device — both sides check, §5.2 defense in depth).
//! - the in-band payload framing ([`TerminalPayload`]) — §5.1: the
//!   format is agent-owned and the server MUST NOT parse it (§5.5);
//!   it is published here so the UI (the other end of the relay) can
//!   speak it. One leading discriminator byte, then the body.

use serde::{Deserialize, Serialize};

/// Bundle-relative path of the terminal enablement config
/// (spec/reeve/03-terminal.md §5.2). Rendered through the overlay
/// tree like any other config; ABSENT file = terminal disabled (a
/// freshly enrolled device MUST refuse opens).
pub const TERMINAL_CONFIG_PATH: &str = "config/terminal.yaml";

/// UI-leg websocket path prefix: `GET
/// /api/reeve/v1/terminal/{sessionId}` (spec/reeve/03-terminal.md
/// §5.1).
pub const TERMINAL_WS_PATH_PREFIX: &str = "/api/reeve/v1/terminal/";

/// RECOMMENDED idle timeout (spec/reeve/03-terminal.md §5.3).
pub const IDLE_TIMEOUT_SECS_DEFAULT: u64 = 5 * 60;

/// RECOMMENDED session hard cap (spec/reeve/03-terminal.md §5.3).
pub const HARD_CAP_SECS_DEFAULT: u64 = 60 * 60;

/// Sub-channel `open.meta` for purpose `rev-002/terminal` — session
/// bootstrap ONLY (spec/reeve/03-terminal.md §5.1): the
/// server-assigned session id, the requested PTY size, and the TERM
/// string. Resize and control ride in-band ([`TerminalPayload`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOpenMeta {
    /// Server-assigned session identifier (§5: a session is
    /// identified by its `sessionId`; reconnection is a NEW session
    /// with a new id, §5.3).
    pub session_id: String,
    /// Requested PTY columns.
    #[serde(default = "default_cols")]
    pub cols: u16,
    /// Requested PTY rows.
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// TERM string for the session's environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

/// The enablement configuration item (spec/reeve/03-terminal.md
/// §5.2), rendered into the device bundle at
/// [`TERMINAL_CONFIG_PATH`]. Every field defaults to the
/// restrictive/RECOMMENDED value; an absent or unparseable file is
/// treated as `enabled: false` (default-deny).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalConfig {
    /// Terminal access switch. DISABLED by default (§5.2). Flipping
    /// this is a tree revision with an author and a diff — there is
    /// no runtime toggle.
    #[serde(default)]
    pub enabled: bool,
    /// Program spawned in the PTY. Default `/bin/sh`. The PTY runs
    /// under the workload-execution identity — the same identity the
    /// agent applies workloads with (§5.3: never an unconstrained
    /// root shell by default; identity is configured in enablement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Idle timeout, seconds (§5.3 RECOMMENDED default 5 min). Both
    /// sides enforce their limits.
    #[serde(default = "default_idle")]
    pub idle_timeout_secs: u64,
    /// Session hard cap, seconds (§5.3 RECOMMENDED default 60 min).
    #[serde(default = "default_cap")]
    pub hard_cap_secs: u64,
}

fn default_idle() -> u64 {
    IDLE_TIMEOUT_SECS_DEFAULT
}

fn default_cap() -> u64 {
    HARD_CAP_SECS_DEFAULT
}

impl Default for TerminalConfig {
    /// The disabled-by-default posture (§5.2): what a device with no
    /// rendered terminal config item evaluates to.
    fn default() -> Self {
        TerminalConfig {
            enabled: false,
            shell: None,
            idle_timeout_secs: IDLE_TIMEOUT_SECS_DEFAULT,
            hard_cap_secs: HARD_CAP_SECS_DEFAULT,
        }
    }
}

/// In-band payload discriminator: raw terminal bytes follow
/// (UI keystrokes toward the agent; PTY output toward the UI).
pub const TERMINAL_FRAME_DATA: u8 = 0;

/// In-band payload discriminator: PTY resize, body = 4 bytes
/// (u16 BE cols, u16 BE rows). UI → agent only.
pub const TERMINAL_FRAME_RESIZE: u8 = 1;

/// One decoded in-band sub-channel payload
/// (spec/reeve/03-terminal.md §5.1: resize and control ride in-band;
/// the encoding is agent-owned — the bridge relays it opaquely,
/// §5.5).
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalPayload<'a> {
    /// Raw terminal bytes.
    Data(&'a [u8]),
    /// Set the PTY size.
    Resize { cols: u16, rows: u16 },
}

/// Encode raw terminal bytes for the sub-channel payload.
pub fn encode_terminal_data(bytes: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + bytes.len());
    buf.push(TERMINAL_FRAME_DATA);
    buf.extend_from_slice(bytes);
    buf
}

/// Encode a resize control message for the sub-channel payload.
pub fn encode_terminal_resize(cols: u16, rows: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5);
    buf.push(TERMINAL_FRAME_RESIZE);
    buf.extend_from_slice(&cols.to_be_bytes());
    buf.extend_from_slice(&rows.to_be_bytes());
    buf
}

/// Decode one in-band payload. `None` for empty payloads, runt
/// resize bodies, or unknown discriminators — receivers ignore what
/// they cannot parse (spec/reeve/01-framework.md §3.4 tolerant
/// reader) rather than killing the session.
pub fn decode_terminal_payload(payload: &[u8]) -> Option<TerminalPayload<'_>> {
    let (&kind, body) = payload.split_first()?;
    match kind {
        TERMINAL_FRAME_DATA => Some(TerminalPayload::Data(body)),
        TERMINAL_FRAME_RESIZE => {
            if body.len() < 4 {
                return None;
            }
            Some(TerminalPayload::Resize {
                cols: u16::from_be_bytes(body[..2].try_into().ok()?),
                rows: u16::from_be_bytes(body[2..4].try_into().ok()?),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_meta_wire_shape() {
        // §5.1: meta carries only sessionId, PTY size, TERM.
        let meta = TerminalOpenMeta {
            session_id: "s-1".into(),
            cols: 120,
            rows: 40,
            term: Some("xterm-256color".into()),
        };
        let wire = serde_json::to_value(&meta).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"sessionId": "s-1", "cols": 120, "rows": 40,
                               "term": "xterm-256color"})
        );
        let de: TerminalOpenMeta = serde_json::from_value(wire).unwrap();
        assert_eq!(de, meta);
        // Size and TERM default when omitted.
        let minimal: TerminalOpenMeta =
            serde_json::from_value(serde_json::json!({"sessionId": "s-2"})).unwrap();
        assert_eq!((minimal.cols, minimal.rows, minimal.term), (80, 24, None));
    }

    #[test]
    fn config_defaults_are_disabled_and_recommended_limits() {
        // §5.2: disabled by default; §5.3 RECOMMENDED limits.
        let cfg = TerminalConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.idle_timeout_secs, 300);
        assert_eq!(cfg.hard_cap_secs, 3600);
        // An empty mapping (file present, no keys) is also disabled.
        let empty: TerminalConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(empty, TerminalConfig::default());
    }

    #[test]
    fn payload_round_trip_and_tolerant_decode() {
        let data = encode_terminal_data(b"ls -la\n");
        assert_eq!(
            decode_terminal_payload(&data),
            Some(TerminalPayload::Data(&b"ls -la\n"[..]))
        );
        let resize = encode_terminal_resize(132, 43);
        assert_eq!(
            decode_terminal_payload(&resize),
            Some(TerminalPayload::Resize { cols: 132, rows: 43 })
        );
        // Empty data body is legal (a zero-byte write).
        assert_eq!(
            decode_terminal_payload(&encode_terminal_data(b"")),
            Some(TerminalPayload::Data(&b""[..]))
        );
        // Unknown discriminator, runt resize, empty payload: ignored.
        assert_eq!(decode_terminal_payload(&[9, 1, 2]), None);
        assert_eq!(decode_terminal_payload(&[TERMINAL_FRAME_RESIZE, 0, 80]), None);
        assert_eq!(decode_terminal_payload(&[]), None);
    }
}
