-- V5: durability & restore verification (C6;
-- spec/reeve/07-durability.md §9.4). Schema law (storage.md D16):
-- explicit PRIMARY KEY on every table; timestamps integer unix seconds.

-- One row per verify-restore run (§9.4: "record the result — when,
-- which generation, last sequence, outcome, failure detail — in the
-- live DB"). Both the `reeve-server verify-restore` subcommand and the
-- scheduled internal task append here; the durability status surface
-- reads the latest row ("last verified restore: <when>"). Retention
-- pruning consults the last generation with outcome='ok' — the last
-- known-verified generation MUST never be pruned (§9.2).
CREATE TABLE verify_restore_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER NOT NULL,
    generation  TEXT,
    last_seq    INTEGER,
    outcome     TEXT NOT NULL CHECK (outcome IN ('ok', 'failed')),
    detail      TEXT
);
