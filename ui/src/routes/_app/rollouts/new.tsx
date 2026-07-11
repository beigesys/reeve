import { useState } from 'react'
import { Link, createFileRoute, useNavigate } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { ArrowLeft, Plus, X } from 'lucide-react'
import { useList } from '@/api/endpoints/devices/devices'
import {
  getListRolloutsQueryKey,
  useCreateRoute,
} from '@/api/endpoints/rollouts/rollouts'
import type { GateSpec, Scope } from '@/api/model'
import { devicesInScope, scopeLabel } from '@/lib/scope'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'

export const Route = createFileRoute('/_app/rollouts/new')({
  component: RolloutCreatePage,
})

type ScopeKind = 'all' | 'fleet' | 'site' | 'type' | 'devices'

/**
 * Create a rollout (REV-010 §11.5): pick the SCOPE to roll the current
 * configuration out to, optionally narrow it by a tag cohort, and set
 * wave/gate parameters. There is no revision to choose — the current
 * desired config is pinned server-side and shipped in waves.
 */
function RolloutCreatePage() {
  const navigate = useNavigate()
  const qc = useQueryClient()
  const create = useCreateRoute()

  const devices = useList()
  const allDevices = devices.data?.status === 200 ? devices.data.data : []

  const distinct = (get: (d: (typeof allDevices)[number]) => string | null | undefined) =>
    [...new Set(allDevices.map(get).filter((v): v is string => !!v))].sort()

  const [scopeKind, setScopeKind] = useState<ScopeKind>('all')
  const [scopeName, setScopeName] = useState('')
  const [deviceFilter, setDeviceFilter] = useState('')
  const [pickedDevices, setPickedDevices] = useState<string[]>([])
  const [tags, setTags] = useState<Record<string, string>>({})
  const [tagKey, setTagKey] = useState('')
  const [tagValue, setTagValue] = useState('')
  const [waveCount, setWaveCount] = useState('')
  const [soakSecs, setSoakSecs] = useState('')
  const [passFraction, setPassFraction] = useState('')
  const [gateTimeoutSecs, setGateTimeoutSecs] = useState('')
  const [undeterminedAllowance, setUndeterminedAllowance] = useState('')
  const [failureThreshold, setFailureThreshold] = useState('')
  const [error, setError] = useState<string | null>(null)

  const nameOptions =
    scopeKind === 'fleet'
      ? distinct((d) => d.fleet)
      : scopeKind === 'site'
        ? distinct((d) => d.site)
        : scopeKind === 'type'
          ? distinct((d) => d.type)
          : []

  const needsName =
    scopeKind === 'fleet' || scopeKind === 'site' || scopeKind === 'type'
  const scopeIncomplete =
    (needsName && scopeName.trim() === '') ||
    (scopeKind === 'devices' && pickedDevices.length === 0)

  const filteredDevices = allDevices.filter((d) => {
    const needle = deviceFilter.toLowerCase()
    return (
      !needle ||
      (d.displayName ?? d.hostname).toLowerCase().includes(needle) ||
      d.deviceId.toLowerCase().includes(needle)
    )
  })

  const toggleDevice = (id: string) =>
    setPickedDevices((prev) =>
      prev.includes(id) ? prev.filter((d) => d !== id) : [...prev, id],
    )

  const num = (s: string): number | null => (s.trim() === '' ? null : Number(s))

  const buildScope = (): Scope => {
    switch (scopeKind) {
      case 'fleet':
        return { kind: 'fleet', name: scopeName.trim() }
      case 'site':
        return { kind: 'site', name: scopeName.trim() }
      case 'type':
        return { kind: 'type', name: scopeName.trim() }
      case 'devices':
        return { kind: 'devices', ids: pickedDevices }
      default:
        return { kind: 'all' }
    }
  }

  const submit = async () => {
    setError(null)
    const gate: GateSpec = {
      soakSecs: num(soakSecs),
      passFraction: passFraction.trim() === '' ? null : Number(passFraction),
      gateTimeoutSecs: num(gateTimeoutSecs),
      undeterminedAllowance: num(undeterminedAllowance),
    }
    const anyGate = Object.values(gate).some((v) => v != null)

    const res = await create.mutateAsync({
      data: {
        scope: buildScope(),
        ...(Object.keys(tags).length > 0 ? { tagCohort: tags } : {}),
        waveCount: num(waveCount),
        failureThreshold: num(failureThreshold),
        ...(anyGate ? { gate } : {}),
      },
    })
    if (res.status === 201) {
      void qc.invalidateQueries({ queryKey: getListRolloutsQueryKey() })
      void navigate({
        to: '/rollouts/$rollout-id',
        params: { 'rollout-id': res.data.rolloutId },
      })
    } else {
      const detail =
        (res.status === 422 || res.status === 409) &&
        res.data &&
        typeof res.data === 'object' &&
        'error' in res.data
          ? String((res.data as { error: unknown }).error)
          : `HTTP ${res.status}`
      setError(detail)
    }
  }

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/rollouts">
            <ArrowLeft className="size-4" />
            Rollouts
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">New rollout</h1>
      </div>

      <div className="flex max-w-3xl flex-col gap-4">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Scope</CardTitle>
            <CardDescription>
              Where to roll out the current configuration, in waves.
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-4">
            <div className="flex flex-wrap items-center gap-3">
              <Select
                value={scopeKind}
                onValueChange={(v) => {
                  setScopeKind(v as ScopeKind)
                  setScopeName('')
                }}
              >
                <SelectTrigger className="w-56">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">All devices</SelectItem>
                  <SelectItem value="fleet">Fleet</SelectItem>
                  <SelectItem value="site">Site</SelectItem>
                  <SelectItem value="type">Device type</SelectItem>
                  <SelectItem value="devices">Specific devices</SelectItem>
                </SelectContent>
              </Select>
              {needsName && (
                <>
                  <Input
                    list="scope-name-options"
                    placeholder={`${scopeKind} name`}
                    value={scopeName}
                    onChange={(e) => setScopeName(e.target.value)}
                    className="w-56"
                  />
                  <datalist id="scope-name-options">
                    {nameOptions.map((o) => (
                      <option key={o} value={o} />
                    ))}
                  </datalist>
                </>
              )}
            </div>

            {scopeKind === 'devices' && (
              <div className="flex flex-col gap-2">
                <Label>Devices ({pickedDevices.length} selected)</Label>
                <Input
                  placeholder="Filter devices…"
                  value={deviceFilter}
                  onChange={(e) => setDeviceFilter(e.target.value)}
                  className="max-w-72"
                />
                <div className="max-h-56 overflow-y-auto rounded-md border p-1">
                  {filteredDevices.length === 0 ? (
                    <p className="px-2 py-1.5 text-sm text-muted-foreground">
                      {devices.isLoading ? 'Loading…' : 'No devices.'}
                    </p>
                  ) : (
                    filteredDevices.map((d) => (
                      <label
                        key={d.deviceId}
                        className="flex cursor-pointer items-center gap-2 rounded px-2 py-1 text-sm hover:bg-accent"
                      >
                        <input
                          type="checkbox"
                          checked={pickedDevices.includes(d.deviceId)}
                          onChange={() => toggleDevice(d.deviceId)}
                          className="accent-primary"
                        />
                        <span>{d.displayName ?? d.hostname}</span>
                        <span className="font-mono text-xs text-muted-foreground">
                          {d.deviceId}
                        </span>
                      </label>
                    ))
                  )}
                </div>
              </div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Tag cohort (optional)</CardTitle>
            <CardDescription>
              Narrow the scope to devices carrying all of these tags.
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-2">
            <div className="flex flex-wrap gap-1">
              {Object.entries(tags).map(([k, v]) => (
                <Badge
                  key={k}
                  variant="secondary"
                  className="gap-1 font-mono font-normal"
                >
                  {k}
                  {v ? `=${v}` : ''}
                  <button
                    type="button"
                    onClick={() =>
                      setTags((prev) => {
                        const next = { ...prev }
                        delete next[k]
                        return next
                      })
                    }
                    aria-label={`Remove ${k}`}
                  >
                    <X className="size-3" />
                  </button>
                </Badge>
              ))}
            </div>
            <form
              className="flex items-center gap-2"
              onSubmit={(e) => {
                e.preventDefault()
                const k = tagKey.trim()
                if (k) setTags((prev) => ({ ...prev, [k]: tagValue.trim() }))
                setTagKey('')
                setTagValue('')
              }}
            >
              <Input
                placeholder="key"
                value={tagKey}
                onChange={(e) => setTagKey(e.target.value)}
                className="max-w-40 font-mono"
              />
              <Input
                placeholder="value"
                value={tagValue}
                onChange={(e) => setTagValue(e.target.value)}
                className="max-w-40 font-mono"
              />
              <Button
                type="submit"
                variant="outline"
                size="sm"
                disabled={!tagKey.trim()}
              >
                <Plus className="size-4" />
              </Button>
            </form>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Waves &amp; gate</CardTitle>
            <CardDescription>
              Blank fields use server defaults. No wave count means one wave
              covering the whole cohort.
            </CardDescription>
          </CardHeader>
          <CardContent className="grid grid-cols-2 gap-4 md:grid-cols-3">
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="wave-count">Wave count</Label>
              <Input
                id="wave-count"
                type="number"
                min={1}
                value={waveCount}
                onChange={(e) => setWaveCount(e.target.value)}
                placeholder="1"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="soak">Soak (seconds)</Label>
              <Input
                id="soak"
                type="number"
                min={0}
                value={soakSecs}
                onChange={(e) => setSoakSecs(e.target.value)}
                placeholder="default"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="pass-fraction">Pass fraction (0–1)</Label>
              <Input
                id="pass-fraction"
                type="number"
                min={0}
                max={1}
                step="0.05"
                value={passFraction}
                onChange={(e) => setPassFraction(e.target.value)}
                placeholder="default"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="gate-timeout">Gate timeout (seconds)</Label>
              <Input
                id="gate-timeout"
                type="number"
                min={0}
                value={gateTimeoutSecs}
                onChange={(e) => setGateTimeoutSecs(e.target.value)}
                placeholder="default"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="undetermined">Undetermined allowance</Label>
              <Input
                id="undetermined"
                type="number"
                min={0}
                value={undeterminedAllowance}
                onChange={(e) => setUndeterminedAllowance(e.target.value)}
                placeholder="unlimited (offline-first)"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="failure-threshold">Failure threshold</Label>
              <Input
                id="failure-threshold"
                type="number"
                min={0}
                value={failureThreshold}
                onChange={(e) => setFailureThreshold(e.target.value)}
                placeholder="default"
              />
            </div>
          </CardContent>
        </Card>

        {!scopeIncomplete &&
          (() => {
            const scopeObj = buildScope()
            const matched = devicesInScope(allDevices, scopeObj).filter((d) =>
              Object.entries(tags).every(
                ([k, v]) => k in d.tags && (v === '' || d.tags[k] === v),
              ),
            )
            const waves = num(waveCount) ?? 1
            return (
              <p className="rounded-md border bg-muted/40 p-3 text-sm">
                Roll out the current configuration to{' '}
                <span className="font-medium">{scopeLabel(scopeObj)}</span>
                {Object.keys(tags).length > 0 && ' (tagged)'} in{' '}
                <span className="font-medium">
                  {waves} wave{waves === 1 ? '' : 's'}
                </span>
                {' — '}
                {matched.length} device{matched.length === 1 ? '' : 's'} match
                right now.
              </p>
            )
          })()}

        <div className="flex items-center gap-3">
          <Button
            onClick={() => void submit()}
            disabled={scopeIncomplete || create.isPending}
          >
            {create.isPending ? 'Creating…' : 'Create rollout'}
          </Button>
          {scopeIncomplete && (
            <span className="text-xs text-muted-foreground">
              {scopeKind === 'devices'
                ? 'Pick at least one device.'
                : `Enter a ${scopeKind} name.`}
            </span>
          )}
          {error && <span className="text-sm text-destructive">{error}</span>}
        </div>
      </div>
    </div>
  )
}
