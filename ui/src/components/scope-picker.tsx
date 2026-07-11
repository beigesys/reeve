import { useEffect, useMemo, useState } from 'react'
import type { DeviceSummary, Scope } from '@/api/model'
import type { ComboboxOption } from '@/components/search-select'
import { SearchSelect } from '@/components/search-select'
import { Checkbox } from '@/components/ui/checkbox'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/tabs'
import { deviceLabel } from '@/lib/scope'
import { useLocationTree } from '@/lib/groups'

/**
 * Segmented scope selector (§11.4): whole fleet, a Fleet, a Site, a
 * Device-type, a tag, or a hand-picked set of devices. Emits the wire
 * `Scope` (or `null` while incomplete). A tag selection resolves to the
 * devices carrying it right now (tag-scoped STAGED delivery is a rollout
 * cohort, not a direct deploy — §11.4), so it becomes a `devices` scope.
 */
export function ScopePicker({
  devices,
  loading,
  allowTag = true,
  onChange,
}: {
  devices: DeviceSummary[]
  loading?: boolean
  allowTag?: boolean
  onChange: (scope: Scope | null) => void
}) {
  type Kind = 'all' | 'fleet' | 'site' | 'type' | 'tag' | 'devices'
  const [kind, setKind] = useState<Kind>('all')
  const [name, setName] = useState('')
  const [tagKey, setTagKey] = useState('')
  const [tagValue, setTagValue] = useState('')
  const [deviceFilter, setDeviceFilter] = useState('')
  const [picked, setPicked] = useState<string[]>([])

  const distinct = (get: (d: DeviceSummary) => string | null | undefined) =>
    [...new Set(devices.map(get).filter((v): v is string => !!v))].sort()

  // Canonical fleet/site groups (§11.1). Sites are listed grouped by their
  // fleet so a scope-by-site name is unambiguous; both are unioned with any
  // value a device already carries so legacy assignments stay selectable.
  const { fleetNames, siteOptions } = useLocationTree()

  const nameOptions: ComboboxOption[] = useMemo(() => {
    const union = (
      canonical: ComboboxOption[],
      observed: string[],
    ): ComboboxOption[] => {
      const seen = new Set(canonical.map((o) => o.value))
      const extra = observed
        .filter((v) => !seen.has(v))
        .map((v) => ({ value: v, label: v }))
      return [...canonical, ...extra].sort((a, b) =>
        a.value.localeCompare(b.value),
      )
    }
    if (kind === 'fleet')
      return union(
        fleetNames.map((n) => ({ value: n, label: n })),
        distinct((d) => d.fleet),
      )
    if (kind === 'site') return union(siteOptions, distinct((d) => d.site))
    if (kind === 'type')
      return distinct((d) => d.type).map((n) => ({ value: n, label: n }))
    return []
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind, fleetNames, siteOptions, devices])

  const tagKeyOptions = useMemo(
    () => [...new Set(devices.flatMap((d) => Object.keys(d.tags)))].sort(),
    [devices],
  )

  // Devices carrying the chosen tag (value optional — key-only matches any).
  const taggedIds = useMemo(() => {
    const k = tagKey.trim()
    if (!k) return []
    return devices
      .filter((d) => {
        if (!(k in d.tags)) return false
        const want = tagValue.trim()
        return want === '' || d.tags[k] === want
      })
      .map((d) => d.deviceId)
  }, [devices, tagKey, tagValue])

  const scope: Scope | null = useMemo(() => {
    switch (kind) {
      case 'all':
        return { kind: 'all' }
      case 'fleet':
        return name.trim() ? { kind: 'fleet', name: name.trim() } : null
      case 'site':
        return name.trim() ? { kind: 'site', name: name.trim() } : null
      case 'type':
        return name.trim() ? { kind: 'type', name: name.trim() } : null
      case 'tag':
        return taggedIds.length ? { kind: 'devices', ids: taggedIds } : null
      case 'devices':
        return picked.length ? { kind: 'devices', ids: picked } : null
    }
  }, [kind, name, taggedIds, picked])

  useEffect(() => {
    onChange(scope)
    // onChange identity is caller-owned; scope drives the effect.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scope])

  const filtered = devices.filter((d) => {
    const needle = deviceFilter.toLowerCase()
    return (
      !needle ||
      deviceLabel(d).toLowerCase().includes(needle) ||
      d.deviceId.toLowerCase().includes(needle)
    )
  })
  const toggle = (id: string) =>
    setPicked((prev) =>
      prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id],
    )

  return (
    <div className="flex flex-col gap-4">
      <Tabs
        value={kind}
        onValueChange={(v) => {
          setKind(v as Kind)
          setName('')
        }}
      >
        <TabsList className="flex-wrap">
          <TabsTrigger value="all">Whole fleet</TabsTrigger>
          <TabsTrigger value="fleet">Fleet</TabsTrigger>
          <TabsTrigger value="site">Site</TabsTrigger>
          <TabsTrigger value="type">Device type</TabsTrigger>
          {allowTag && <TabsTrigger value="tag">Tag</TabsTrigger>}
          <TabsTrigger value="devices">Pick devices</TabsTrigger>
        </TabsList>
      </Tabs>

      {(kind === 'fleet' || kind === 'site' || kind === 'type') && (
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="scope-name">
            {kind === 'fleet'
              ? 'Fleet name'
              : kind === 'site'
                ? 'Site name'
                : 'Device type'}
          </Label>
          <SearchSelect
            id="scope-name"
            value={name}
            onChange={setName}
            options={nameOptions}
            placeholder={`Select ${kind}…`}
            emptyText={`No ${kind} groups yet.`}
            clearable
            className="max-w-72"
          />
        </div>
      )}

      {kind === 'tag' && (
        <div className="flex flex-col gap-1.5">
          <Label>Tag</Label>
          <div className="flex items-center gap-2">
            <Input
              list="tag-key-options"
              placeholder="key"
              value={tagKey}
              onChange={(e) => setTagKey(e.target.value)}
              className="max-w-44 font-mono"
            />
            <span className="text-muted-foreground">=</span>
            <Input
              placeholder="value (any)"
              value={tagValue}
              onChange={(e) => setTagValue(e.target.value)}
              className="max-w-44 font-mono"
            />
          </div>
          <datalist id="tag-key-options">
            {tagKeyOptions.map((k) => (
              <option key={k} value={k} />
            ))}
          </datalist>
          <span className="text-xs text-muted-foreground">
            Targets the devices carrying this tag right now
            {tagKey.trim() ? ` — ${taggedIds.length} match` : ''}.
          </span>
        </div>
      )}

      {kind === 'devices' && (
        <div className="flex flex-col gap-2">
          <Label>Devices ({picked.length} selected)</Label>
          <Input
            placeholder="Filter devices…"
            value={deviceFilter}
            onChange={(e) => setDeviceFilter(e.target.value)}
            className="max-w-72"
          />
          <div className="max-h-56 overflow-y-auto rounded-md border p-1">
            {filtered.length === 0 ? (
              <p className="px-2 py-1.5 text-sm text-muted-foreground">
                {loading ? 'Loading…' : 'No devices.'}
              </p>
            ) : (
              filtered.map((d) => (
                <button
                  type="button"
                  key={d.deviceId}
                  onClick={() => toggle(d.deviceId)}
                  className="flex w-full cursor-pointer items-center gap-2 rounded px-2 py-1 text-left text-sm hover:bg-accent"
                >
                  <Checkbox
                    checked={picked.includes(d.deviceId)}
                    tabIndex={-1}
                    className="pointer-events-none"
                  />
                  <span>{deviceLabel(d)}</span>
                  <span className="font-mono text-xs text-muted-foreground">
                    {d.deviceId}
                  </span>
                </button>
              ))
            )}
          </div>
        </div>
      )}
    </div>
  )
}
