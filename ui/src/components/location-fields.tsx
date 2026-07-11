import { useGroupsList } from '@/api/endpoints/groups/groups'
import { SearchSelect } from '@/components/search-select'
import { Label } from '@/components/ui/label'

/**
 * Cascading Fleet -> Site assignment fields (spec/reeve/11-fleet-model.md
 * §11.1). Fleet and Site are a containment tree, not two free axes: the
 * Site options come ONLY from the SELECTED fleet's sites, fetched lazily
 * via the scoped GET /api/groups?kind=site&fleet=<name> read. Changing the
 * fleet clears/rescopes the site. A brand-new fleet/site can be typed
 * (creatable); the caller is responsible for creating the groups (device
 * PATCH via `ensureLocationGroups`, enrollment via server auto-provision).
 * This makes a wrong-fleet site un-pickable — the fix for the "mixed" bug.
 *
 * Device-type is orthogonal (§11.1) and is NOT rendered here — it stays a
 * free creatable field at the call site.
 */
export function LocationFields({
  fleet,
  site,
  onFleetChange,
  onSiteChange,
  idPrefix = 'loc',
  disabled = false,
}: {
  fleet: string
  site: string
  onFleetChange: (v: string) => void
  onSiteChange: (v: string) => void
  idPrefix?: string
  disabled?: boolean
}) {
  const tree = useGroupsList(undefined)
  const fleetNames =
    tree.data?.status === 200 ? tree.data.data.fleets.map((f) => f.name) : []

  // Scoped, lazy: only fetch the chosen fleet's sites once a fleet is set.
  const scoped = useGroupsList(
    { kind: 'site', fleet },
    { query: { enabled: !!fleet.trim() } },
  )
  const siteNames =
    fleet.trim() && scoped.data?.status === 200
      ? (scoped.data.data.fleets[0]?.sites ?? []).map((s) => s.name)
      : []

  return (
    <>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor={`${idPrefix}-fleet`}>Fleet</Label>
        <SearchSelect
          id={`${idPrefix}-fleet`}
          value={fleet}
          onChange={(v) => {
            // Changing the fleet rescopes the site; a stranded site is
            // invalid under containment, so clear it.
            if (v !== fleet) onSiteChange('')
            onFleetChange(v)
          }}
          options={fleetNames.map((o) => ({ value: o, label: o }))}
          placeholder="Unassigned"
          emptyText="Type to add a new fleet."
          creatable
          clearable
          disabled={disabled}
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor={`${idPrefix}-site`}>Site</Label>
        <SearchSelect
          id={`${idPrefix}-site`}
          value={site}
          onChange={onSiteChange}
          options={siteNames.map((o) => ({ value: o, label: o }))}
          placeholder={fleet.trim() ? 'No site' : 'Choose a fleet first'}
          emptyText={
            scoped.isLoading ? 'Loading…' : 'Type to add a site to this fleet.'
          }
          creatable
          clearable
          disabled={disabled || !fleet.trim()}
        />
      </div>
    </>
  )
}
