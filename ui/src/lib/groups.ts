// Location-group helpers (spec/reeve/11-fleet-model.md §11.1/§11.3).
// Fleet -> Site is a containment tree: a Site belongs to exactly one
// Fleet. These read the canonical `location_groups` tree from
// GET /api/groups and provide the scoped, cascading option lists the
// assignment/scope pickers need. Presentation only — the wire types and
// hooks come from ui/src/api (CLAUDE.md ui/ rule).
import type { QueryClient } from '@tanstack/react-query'
import {
  groupsCreate,
  groupsList,
  getGroupsListQueryKey,
  useGroupsList,
} from '@/api/endpoints/groups/groups'
import type { ComboboxOption } from '@/components/search-select'

/** A best-effort human string out of an API error body. */
function errText(body: unknown): string | null {
  if (body && typeof body === 'object' && 'error' in body) {
    return String((body as { error: unknown }).error)
  }
  return null
}

/**
 * The full canonical Fleet -> Site tree (all fleets, each with its sites).
 * Used to seed fleet option lists and the "all sites, grouped by fleet"
 * option list the deploy/rollout scope pickers show.
 */
export function useLocationTree() {
  const q = useGroupsList(undefined)
  const tree = q.data?.status === 200 ? q.data.data : undefined
  const fleets = tree?.fleets ?? []
  const fleetNames = fleets.map((f) => f.name)

  // All sites across every fleet, deduplicated by name. A site name can
  // recur under different fleets (they are distinct sites); the label
  // carries the containing fleet(s) so the operator can tell them apart.
  const byName = new Map<string, Set<string>>()
  for (const f of fleets) {
    for (const s of f.sites) {
      const set = byName.get(s.name) ?? new Set<string>()
      set.add(f.name)
      byName.set(s.name, set)
    }
  }
  const siteOptions: ComboboxOption[] = [...byName.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([name, fleetsFor]) => ({
      value: name,
      label: `${name} (${[...fleetsFor].sort().join(', ')})`,
    }))

  return { tree, fleets, fleetNames, siteOptions, isLoading: q.isLoading }
}

/**
 * Ensure the (fleet, site) pair exists as canonical groups before a strict
 * device PATCH (which validates the assignment against the tree). Idempotent:
 * a fleet that already exists returns 409, which we treat as success and
 * resolve its id via a scoped read; a duplicate site is likewise fine. The
 * site is always created UNDER the fleet, never orphaned (§11.1). Returns a
 * human error string on a real failure (e.g. an invalid name), else null.
 */
export async function ensureLocationGroups(
  fleet: string,
  site: string,
  qc: QueryClient,
): Promise<string | null> {
  const f = fleet.trim()
  const s = site.trim()
  if (!f) return null // no fleet -> nothing to contain (clearing is allowed)

  let fleetId: number | undefined
  const cr = await groupsCreate({ kind: 'fleet', name: f })
  if (cr.status === 201) {
    fleetId = cr.data.id
  } else if (cr.status === 409) {
    // Already exists — resolve its id via a scoped read.
    const t = await groupsList({ fleet: f })
    if (t.status === 200) fleetId = t.data.fleets[0]?.id
  } else if (cr.status === 422) {
    return errText(cr.data) ?? `Could not create fleet "${f}".`
  }

  if (s && fleetId != null) {
    const sc = await groupsCreate({ kind: 'site', name: s, parentId: fleetId })
    if (sc.status === 422) {
      return errText(sc.data) ?? `Could not create site "${s}".`
    }
    // 201 created, 409 already present — both fine.
  }

  // Prefix match invalidates the full tree and every scoped read.
  await qc.invalidateQueries({ queryKey: getGroupsListQueryKey() })
  return null
}
