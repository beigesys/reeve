// Scope helpers (spec/reeve/11-fleet-model.md §11.4). A deploy/rollout
// targets a scope; the UI resolves the same wire `Scope` union and can
// preview which devices a scope currently covers. Presentation only —
// the wire type comes from ui/src/api (CLAUDE.md ui/ rule).
import type { DeviceSummary, Scope } from '@/api/model'

/** Human phrasing for a scope, e.g. `Site plant-a`, `4 devices`. */
export function scopeLabel(scope: Scope): string {
  switch (scope.kind) {
    case 'all':
      return 'All devices'
    case 'fleet':
      return `Fleet ${scope.name}`
    case 'site':
      return `Site ${scope.name}`
    case 'type':
      return `Type ${scope.name}`
    case 'devices':
      return `${scope.ids.length} device${scope.ids.length === 1 ? '' : 's'}`
  }
}

/** Whether a device is currently covered by a scope (for live preview). */
export function deviceInScope(d: DeviceSummary, scope: Scope): boolean {
  switch (scope.kind) {
    case 'all':
      return true
    case 'fleet':
      return d.fleet === scope.name
    case 'site':
      return d.site === scope.name
    case 'type':
      return d.type === scope.name
    case 'devices':
      return scope.ids.includes(d.deviceId)
  }
}

/** Devices a scope currently covers, given the live device list. */
export function devicesInScope(
  devices: DeviceSummary[],
  scope: Scope,
): DeviceSummary[] {
  return devices.filter((d) => deviceInScope(d, scope))
}

/** A device's display label (rename wins over hostname). */
export function deviceLabel(d: DeviceSummary): string {
  return d.displayName ?? d.hostname
}
