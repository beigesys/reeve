-- V2: enrollment (docs/decisions/agent.md D4, auth.md D1,
-- tree-render.md D11/D12). Schema law (storage.md D16): explicit
-- PRIMARY KEY on every table. Timestamps are integer unix seconds.

-- Operator-created join tokens (D4): TTL + max-uses, stored hashed
-- (hex sha256 — sufficient for random 256-bit tokens). device_id set
-- => a re-enroll token bound to that existing device; a fresh box
-- presenting it resumes the old identity and desired state.
CREATE TABLE join_tokens (
    token_hash TEXT PRIMARY KEY,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    max_uses   INTEGER NOT NULL,
    uses       INTEGER NOT NULL DEFAULT 0,
    device_id  TEXT REFERENCES devices(device_id) ON DELETE CASCADE,
    revoked_at INTEGER
);

-- Device row shape for enrollment + the render layer chain:
-- labels are free-form (JSON object; D12: labels group, layers
-- configure — labels MUST NOT select or inject configuration);
-- class/region/site are the device's layer chain membership
-- (D11: fleet -> class? -> region -> site -> device), nullable;
-- stale marks an identity superseded by a plain-token enrollment
-- from the same hostname (D4 wiped-box case);
-- enrolled_with is the hash of the join token that produced this
-- enrollment — the idempotency key for a retried install (D4).
ALTER TABLE devices ADD COLUMN labels        TEXT NOT NULL DEFAULT '{}';
ALTER TABLE devices ADD COLUMN class         TEXT;
ALTER TABLE devices ADD COLUMN region        TEXT;
ALTER TABLE devices ADD COLUMN site          TEXT;
ALTER TABLE devices ADD COLUMN last_seen_at  INTEGER;
ALTER TABLE devices ADD COLUMN stale         INTEGER NOT NULL DEFAULT 0;
ALTER TABLE devices ADD COLUMN enrolled_with TEXT;

CREATE INDEX devices_hostname_idx ON devices (hostname);
CREATE INDEX devices_enrolled_with_idx ON devices (enrolled_with);
