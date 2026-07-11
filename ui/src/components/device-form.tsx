import { useMemo, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Pin, PinOff, X } from 'lucide-react'
import {
  getDetailQueryKey,
  getListQueryKey,
  useDecommission,
  usePatch,
  useList,
} from '@/api/endpoints/devices/devices'
import type { DeviceDetail, PatchDeviceRequest } from '@/api/model'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { ConfirmButton } from '@/components/confirm-button'

type Tags = Record<string, string>

/** Distinct non-empty values of a column across every known device. */
function useGroupOptions() {
  const devices = useList()
  const data = devices.data
  return useMemo(() => {
    const rows = data?.status === 200 ? data.data : []
    const pick = (get: (d: (typeof rows)[number]) => string | null | undefined) => {
      const set = new Set<string>()
      for (const d of rows) {
        const v = get(d)
        if (v) set.add(v)
      }
      return [...set].sort()
    }
    return {
      fleet: pick((d) => d.fleet),
      site: pick((d) => d.site),
      type: pick((d) => d.type),
    }
  }, [data])
}

/**
 * A group-assignment field: pick an existing group name or type a new
 * one (native datalist = suggestions + free add, no bespoke combobox).
 */
function MoveField({
  id,
  label,
  value,
  options,
  onChange,
}: {
  id: string
  label: string
  value: string
  options: string[]
  onChange: (v: string) => void
}) {
  const listId = `${id}-options`
  return (
    <div className="flex flex-col gap-1.5">
      <Label htmlFor={id}>{label}</Label>
      <Input
        id={id}
        list={listId}
        value={value}
        placeholder="Unassigned"
        onChange={(e) => onChange(e.target.value)}
      />
      <datalist id={listId}>
        {options.map((o) => (
          <option key={o} value={o} />
        ))}
      </datalist>
    </div>
  )
}

/**
 * The one shared device-management form (spec/reeve/11-fleet-model.md
 * §11.3): rename, move between groups, edit tags, pin, decommission.
 * Reused wherever a device is edited. Sends only changed fields (an
 * empty rename/group clears it; empty tag set clears all tags), then
 * invalidates the device queries so the fleet updates live.
 */
export function DeviceForm({ device }: { device: DeviceDetail }) {
  const navigate = useNavigate()
  const qc = useQueryClient()
  const patch = usePatch()
  const decommission = useDecommission()
  const options = useGroupOptions()

  const [displayName, setDisplayName] = useState(device.displayName ?? '')
  const [fleet, setFleet] = useState(device.fleet ?? '')
  const [site, setSite] = useState(device.site ?? '')
  const [type, setType] = useState(device.type ?? '')
  const [pinned, setPinned] = useState(device.pinned)
  const [tags, setTags] = useState<Tags>({ ...device.tags })
  const [tagKey, setTagKey] = useState('')
  const [tagValue, setTagValue] = useState('')
  const [error, setError] = useState<string | null>(null)

  const addTag = () => {
    const k = tagKey.trim()
    if (k === '') return
    setTags((prev) => ({ ...prev, [k]: tagValue.trim() }))
    setTagKey('')
    setTagValue('')
  }
  const removeTag = (k: string) =>
    setTags((prev) => {
      const next = { ...prev }
      delete next[k]
      return next
    })

  const invalidate = () => {
    void qc.invalidateQueries({ queryKey: getListQueryKey() })
    void qc.invalidateQueries({ queryKey: getDetailQueryKey(device.deviceId) })
  }

  // Only changed fields go in the patch: absent = unchanged, null = clear.
  const buildPatch = (): PatchDeviceRequest => {
    const body: PatchDeviceRequest = {}
    const norm = (s: string) => (s.trim() === '' ? null : s.trim())
    const cur = (v: string | null | undefined) => v ?? null
    if (norm(displayName) !== cur(device.displayName))
      body.displayName = norm(displayName)
    if (norm(fleet) !== cur(device.fleet)) body.fleet = norm(fleet)
    if (norm(site) !== cur(device.site)) body.site = norm(site)
    if (norm(type) !== cur(device.type)) body.type = norm(type)
    if (pinned !== device.pinned) body.pinned = pinned
    const tagsChanged =
      JSON.stringify(sortEntries(tags)) !== JSON.stringify(sortEntries(device.tags))
    if (tagsChanged) body.tags = Object.keys(tags).length === 0 ? null : tags
    return body
  }

  const save = async () => {
    setError(null)
    const body = buildPatch()
    if (Object.keys(body).length === 0) {
      backToDetail()
      return
    }
    const res = await patch.mutateAsync({ deviceId: device.deviceId, data: body })
    if (res.status === 200) {
      invalidate()
      backToDetail()
    } else if (res.status === 404) {
      setError('This device no longer exists.')
    } else {
      setError(`Could not save changes (HTTP ${res.status}).`)
    }
  }

  const backToDetail = () =>
    navigate({
      to: '/devices/$device-id',
      params: { 'device-id': device.deviceId },
    })

  const doDecommission = async () => {
    setError(null)
    const res = await decommission.mutateAsync({ deviceId: device.deviceId })
    if (res.status === 204) {
      invalidate()
      navigate({ to: '/devices' })
    } else if (res.status === 404) {
      // Already gone — treat as done.
      invalidate()
      navigate({ to: '/devices' })
    } else {
      setError(`Could not decommission (HTTP ${res.status}).`)
    }
  }

  const tagEntries = Object.entries(tags).sort(([a], [b]) => a.localeCompare(b))

  return (
    <div className="flex max-w-2xl flex-col gap-6">
      {/* Rename */}
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="display-name">Display name</Label>
        <Input
          id="display-name"
          value={displayName}
          placeholder={device.hostname}
          onChange={(e) => setDisplayName(e.target.value)}
        />
        <span className="text-xs text-muted-foreground">
          A friendly name. Leave blank to show the hostname ({device.hostname}).
        </span>
      </div>

      {/* Move between groups */}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
        <MoveField
          id="fleet"
          label="Fleet"
          value={fleet}
          options={options.fleet}
          onChange={setFleet}
        />
        <MoveField
          id="site"
          label="Site"
          value={site}
          options={options.site}
          onChange={setSite}
        />
        <MoveField
          id="type"
          label="Device type"
          value={type}
          options={options.type}
          onChange={setType}
        />
      </div>
      <span className="-mt-4 text-xs text-muted-foreground">
        Moving a device updates the configuration it receives.
      </span>

      {/* Tags */}
      <div className="flex flex-col gap-2">
        <Label>Tags</Label>
        {tagEntries.length === 0 ? (
          <span className="text-sm text-muted-foreground">No tags.</span>
        ) : (
          <div className="flex flex-wrap gap-1.5">
            {tagEntries.map(([k, v]) => (
              <Badge
                key={k}
                variant="secondary"
                className="gap-1 font-mono text-xs font-normal"
              >
                {k}
                {v ? `=${v}` : ''}
                <button
                  type="button"
                  aria-label={`Remove tag ${k}`}
                  className="ml-0.5 rounded-sm text-muted-foreground hover:text-foreground"
                  onClick={() => removeTag(k)}
                >
                  <X className="size-3" />
                </button>
              </Badge>
            ))}
          </div>
        )}
        <div className="flex flex-wrap items-end gap-2">
          <div className="flex flex-col gap-1">
            <Label htmlFor="tag-key" className="text-xs text-muted-foreground">
              Key
            </Label>
            <Input
              id="tag-key"
              value={tagKey}
              className="w-40"
              onChange={(e) => setTagKey(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault()
                  addTag()
                }
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="tag-value" className="text-xs text-muted-foreground">
              Value
            </Label>
            <Input
              id="tag-value"
              value={tagValue}
              className="w-40"
              onChange={(e) => setTagValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault()
                  addTag()
                }
              }}
            />
          </div>
          <Button
            type="button"
            variant="outline"
            onClick={addTag}
            disabled={tagKey.trim() === ''}
          >
            Add tag
          </Button>
        </div>
        <span className="text-xs text-muted-foreground">
          Tags group and filter devices; they never change configuration.
        </span>
      </div>

      {/* Pin */}
      <div className="flex flex-col gap-2">
        <Label>Pin</Label>
        <div className="flex items-center gap-3">
          <Button
            type="button"
            variant={pinned ? 'default' : 'outline'}
            onClick={() => setPinned((p) => !p)}
          >
            {pinned ? (
              <>
                <Pin className="size-4" /> Pinned
              </>
            ) : (
              <>
                <PinOff className="size-4" /> Not pinned
              </>
            )}
          </Button>
          <span className="text-xs text-muted-foreground">
            A pinned device holds its current configuration and is left out of
            new deploys and rollouts until unpinned.
          </span>
        </div>
      </div>

      {error && <p className="text-sm text-destructive">{error}</p>}

      {/* Actions */}
      <div className="flex items-center justify-between gap-3 border-t pt-4">
        <div className="flex items-center gap-2">
          <Button onClick={() => void save()} disabled={patch.isPending}>
            {patch.isPending ? 'Saving…' : 'Save changes'}
          </Button>
          <Button variant="ghost" onClick={backToDetail}>
            Cancel
          </Button>
        </div>
        <ConfirmButton
          label="Decommission"
          confirmLabel={`Decommission ${device.displayName ?? device.hostname}?`}
          description="This revokes the device's credential and stops serving its configuration. The device can no longer reach the server. This cannot be undone."
          onConfirm={() => void doDecommission()}
          disabled={decommission.isPending}
        />
      </div>
    </div>
  )
}

function sortEntries(t: Tags): [string, string][] {
  return Object.entries(t).sort(([a], [b]) => a.localeCompare(b))
}
