-- V11: canonical location groups + fleet->site containment (REV-010
-- amendment; spec/reeve/11-fleet-model.md §11.1/§11.3). The FIX for the
-- "mixed" bug: fleet/site were independent free-text columns, so a device
-- could be assigned to a site that does not belong to its fleet. Here
-- fleet->site becomes a real CONTAINMENT TREE — a Site belongs to exactly
-- one Fleet — and device assignments are validated against it going
-- forward. Device-type stays an ORTHOGONAL free column (devices."type"),
-- NOT a group (a "sensor" type may apply at any site). Tags stay free.
--
-- Schema law (storage.md D16): explicit PRIMARY KEY on every table;
-- timestamps integer unix seconds. Additive only — devices.fleet /
-- devices.site (the name columns) REMAIN the source of truth for the
-- layer chain; this table is the canonical set they are validated
-- against, not a schema change to devices.

CREATE TABLE location_groups (
    group_id   INTEGER PRIMARY KEY,
    kind       TEXT    NOT NULL CHECK (kind IN ('fleet', 'site')),
    name       TEXT    NOT NULL,
    -- A fleet is top-level (parent_id NULL); a site is contained by
    -- exactly one fleet (parent_id => that fleet). ON DELETE RESTRICT so a
    -- fleet with child sites cannot be deleted out from under them (the
    -- app layer refuses first with a clearer message).
    parent_id  INTEGER REFERENCES location_groups(group_id) ON DELETE RESTRICT,
    created_at INTEGER NOT NULL DEFAULT 0,
    -- Containment shape enforced at the storage layer: an orphaned site or
    -- a parented fleet is impossible, not merely discouraged in code.
    CHECK (
        (kind = 'fleet' AND parent_id IS NULL) OR
        (kind = 'site'  AND parent_id IS NOT NULL)
    )
);

-- Fleet names are globally unique. A plain UNIQUE(kind, name, parent_id)
-- would NOT catch two fleets: SQLite treats NULLs as distinct in a UNIQUE
-- index, and every fleet has parent_id NULL. Hence a dedicated partial
-- index over the fleet rows.
CREATE UNIQUE INDEX location_groups_fleet_uniq
    ON location_groups (name) WHERE kind = 'fleet';

-- A site name is unique WITHIN its fleet (the SAME site name may recur
-- under a different fleet — §11.1: uniqueness is per-fleet, so "plant-a"
-- under fleet north and "plant-a" under fleet south are distinct sites).
CREATE UNIQUE INDEX location_groups_site_uniq
    ON location_groups (parent_id, name) WHERE kind = 'site';

CREATE INDEX location_groups_parent_idx ON location_groups (parent_id);

-- Backfill: promote the existing free-text devices.fleet and
-- (fleet, site) pairs into canonical groups so today's assignments stay
-- valid under the new containment rule. A device carrying a site but no
-- fleet cannot be contained; its site column is left untouched (no group)
-- — a pre-existing misconfiguration the operator resolves by assigning a
-- fleet, at which point the site must exist under it.
INSERT INTO location_groups (kind, name, parent_id)
SELECT DISTINCT 'fleet', fleet, NULL
FROM devices
WHERE fleet IS NOT NULL AND fleet <> '';

INSERT INTO location_groups (kind, name, parent_id)
SELECT DISTINCT 'site', d.site, f.group_id
FROM devices d
JOIN location_groups f ON f.kind = 'fleet' AND f.name = d.fleet
WHERE d.site IS NOT NULL AND d.site <> ''
  AND d.fleet IS NOT NULL AND d.fleet <> '';
