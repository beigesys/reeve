-- V10: operator fleet model (REV-010; spec/reeve/11-fleet-model.md
-- §11.1/§11.3/§11.6). Schema law (storage.md D16): explicit PRIMARY KEY
-- on every table; timestamps integer unix seconds. Additive only —
-- class/region (V2) are RETAINED but no longer participate in the render
-- layer chain (the taxonomy remapped to all/fleet/site/type/device);
-- dropping them is unnecessary and non-crash-only, so they simply lie
-- dormant.

-- Assignment tiers (§11.1): a device's config layer chain is
-- 00-all + 10-fleet.<fleet>? + 20-site.<site>? + 30-type.<type>? +
-- 40-device.<id>. `fleet` and `type` join the pre-existing `site` (V2);
-- all three are nullable (an unassigned level is absent from the chain,
-- D12). Changing any of them re-renders the device (render.rs
-- ensure_current) because its layer chain moved.
ALTER TABLE devices ADD COLUMN fleet        TEXT;
ALTER TABLE devices ADD COLUMN "type"       TEXT;

-- Human rename, distinct from the immutable device_id (§11.3). NULL =>
-- fall back to hostname/device_id in the UI.
ALTER TABLE devices ADD COLUMN display_name TEXT;

-- Pin (§11.3): a boolean hold. A pinned device keeps its current
-- desired config and is excluded from new deploys/rollouts until
-- unpinned (render.rs holds its manifest at its current rendered
-- revision; ext/rollouts.rs excludes it from cohort selection). It
-- still counts as converged in gate math (09-rollouts D12).
ALTER TABLE devices ADD COLUMN pinned       INTEGER NOT NULL DEFAULT 0;

-- Decommission tombstone (§11.3): set => the device credential was
-- revoked and its desired state stops being served (the manifest
-- endpoint 404s it, and the render pass skips it). Idempotent: a
-- second decommission is a no-op. NULL = active.
ALTER TABLE devices ADD COLUMN decommissioned_at INTEGER;

-- Enrollment pre-assignment (§11.3, agent.md D4): a join token MAY
-- carry an assignment (fleet/site/type) and tags applied to the device
-- row at first contact, so a box lands in the right group immediately.
-- tags is a JSON object (same shape as devices.labels); NULL => none.
ALTER TABLE join_tokens ADD COLUMN fleet TEXT;
ALTER TABLE join_tokens ADD COLUMN site  TEXT;
ALTER TABLE join_tokens ADD COLUMN "type" TEXT;
ALTER TABLE join_tokens ADD COLUMN tags  TEXT;
