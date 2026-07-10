-- V8: staged rollouts (C9; spec/reeve/09-rollouts.md REV-008,
-- docs/decisions/tree-render.md D12). Schema law (storage.md D16):
-- explicit PRIMARY KEY on every table; timestamps integer unix seconds.
--
-- Tables exist regardless of the ext-rollouts feature (same rule as
-- V6/V7): schema is stable across feature sets. A core
-- (--no-default-features) binary honors existing device_render_targets
-- rows in its render pipeline, so a rollout paused by a full binary
-- remains the stable, inspectable position §11.2 requires even after a
-- feature downgrade — nothing silently jumps to head.

-- Per-device render target (the rollout "hold"): while a row exists,
-- the render pipeline (render.rs) renders this device against
-- `revision` instead of the local head. Rollouts stage manifest
-- advancement by moving these rows (§11.2: a device's desired state is
-- whatever its manifest points at, so controlling propagation IS
-- controlling manifest advancement); deleting a device's row returns
-- it to head-tracking. CORE-honored; ext-rollouts is the only writer.
CREATE TABLE device_render_targets (
    device_id  TEXT PRIMARY KEY
                   REFERENCES devices(device_id) ON DELETE CASCADE,
    revision   INTEGER NOT NULL,
    rollout_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- One rollout (§11.1): definition + current state. The definition
-- (revision, cohort, gate policy, failure threshold) is immutable
-- after creation; only state / current_wave / pause_reason move.
-- Irreplaceable-in-flight runtime state (07-durability §9.5).
--   state      — active | paused | aborted | completed. Aborting is
--                pausing permanently (§11.2); records are retained and
--                holds persist (nothing ever moves backward, §11.5).
--   pass_fraction / undetermined_allowance / soak+timeout — §11.3 gate
--                policy (allowance NULL = the whole wave may be
--                undetermined, the Law-5-friendly default).
--   failure_threshold — §11.4 (default 1: any failed device pauses).
CREATE TABLE rollouts (
    rollout_id              TEXT PRIMARY KEY,
    revision                INTEGER NOT NULL,
    state                   TEXT NOT NULL
        CHECK (state IN ('active', 'paused', 'aborted', 'completed')),
    current_wave            INTEGER NOT NULL DEFAULT 0,
    cohort_json             TEXT NOT NULL,
    soak_secs               INTEGER NOT NULL,
    gate_timeout_secs       INTEGER NOT NULL,
    pass_fraction           REAL NOT NULL,
    undetermined_allowance  INTEGER,
    failure_threshold       INTEGER NOT NULL,
    pause_reason            TEXT,
    created_by              TEXT NOT NULL,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL
);

-- Ordered waves (§11.1: explicit device sets resolved at creation).
--   state — pending | advancing | soaking | passed | failed.
--   gate_json — the recorded gate evaluation (§11.3 counts + verdict,
--   including the undetermined set).
CREATE TABLE rollout_waves (
    rollout_id      TEXT NOT NULL
                        REFERENCES rollouts(rollout_id) ON DELETE CASCADE,
    wave_idx        INTEGER NOT NULL,
    state           TEXT NOT NULL
        CHECK (state IN ('pending', 'advancing', 'soaking', 'passed', 'failed')),
    soak_started_at INTEGER,
    gated_at        INTEGER,
    gate_json       TEXT,
    PRIMARY KEY (rollout_id, wave_idx)
);

-- Per-device rollout assignment + advancement bookkeeping.
--   baseline_revision — where the device is held until its wave
--       advances it (§11.2: devices not yet advanced stay on the old).
--   advanced/advanced_at — set in the SAME transaction as the target
--       move (§11.2 per-device atomic advancement; a crash mid-wave
--       leaves a resumable position, not corruption).
--   unaffected — D12/§11.1: the device's render at the rollout
--       revision is materially unchanged (byte-identical content
--       digest — e.g. a device-layer pin overrides the change). Counts
--       as CONVERGED in gate math and is surfaced as
--       "pinned/unaffected: N" in the status API.
CREATE TABLE rollout_devices (
    rollout_id        TEXT NOT NULL
                          REFERENCES rollouts(rollout_id) ON DELETE CASCADE,
    device_id         TEXT NOT NULL
                          REFERENCES devices(device_id) ON DELETE CASCADE,
    wave_idx          INTEGER NOT NULL,
    baseline_revision INTEGER NOT NULL,
    advanced          INTEGER NOT NULL DEFAULT 0,
    advanced_at       INTEGER,
    unaffected        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (rollout_id, device_id)
);
CREATE INDEX rollout_devices_device_idx ON rollout_devices (device_id);

-- Full transition history (§11.1: "rollout definitions and their full
-- transition history are runtime state"; §11.8: creating, resuming and
-- aborting are attributable, audit-logged operations — author is the
-- authenticated identity for human actions, 'engine' for automatic
-- transitions such as auto-pause).
CREATE TABLE rollout_transitions (
    rollout_id TEXT NOT NULL
                   REFERENCES rollouts(rollout_id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    ts         INTEGER NOT NULL,
    action     TEXT NOT NULL,
    author     TEXT NOT NULL,
    detail     TEXT,
    PRIMARY KEY (rollout_id, seq)
);
