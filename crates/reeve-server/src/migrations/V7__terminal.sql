-- V7: remote terminal session audit (C8; spec/reeve/03-terminal.md
-- §5.4). Schema law (storage.md D16): explicit PRIMARY KEY on every
-- table; timestamps integer unix seconds.
--
-- Like V6, this table exists regardless of the ext-terminal cargo
-- feature: schema is stable across feature sets so a
-- --no-default-features binary can still restore/verify (and finalize
-- dangling rows from) a database written by a full one.

-- One row per terminal session — written at initiation, BEFORE any
-- bytes flow, finalized at close (§5.4). Denied initiations get a row
-- too (started_at == ended_at, close_reason = the denial).
--   session_id          — server-assigned id; reconnection is a NEW
--                         session with a new id and a new row (§5.3).
--   username            — the authenticated human who initiated
--                         (docs/decisions/auth.md D1: terminal only
--                         under password/proxy modes, so a username
--                         always exists).
--   started_at          — initiation (audit-before-bytes, §5.4).
--   opened_at           — agent accepted the sub-channel; NULL when
--                         the session never opened (denied/rejected).
--   ended_at            — finalization; NULL ONLY while the session is
--                         live. Crash recovery: startup closes any
--                         NULL ended_at as close_reason
--                         'server-restart' (§5.4, Law 3).
--   bytes_up/bytes_down — relay accounting, never content (§5.5: the
--                         bridge MAY count bytes; it MUST NOT log
--                         session content).
--   enablement_revision — the local-stream revision id of the render
--                         in effect when the session was authorized
--                         (§5.4 "the enablement commit id in effect").
CREATE TABLE terminal_sessions (
    session_id          TEXT PRIMARY KEY,
    device_id           TEXT NOT NULL,
    username            TEXT NOT NULL,
    started_at          INTEGER NOT NULL,
    opened_at           INTEGER,
    ended_at            INTEGER,
    close_reason        TEXT,
    bytes_up            INTEGER NOT NULL DEFAULT 0,
    bytes_down          INTEGER NOT NULL DEFAULT 0,
    enablement_revision INTEGER
);

CREATE INDEX terminal_sessions_by_device
    ON terminal_sessions (device_id, started_at);
