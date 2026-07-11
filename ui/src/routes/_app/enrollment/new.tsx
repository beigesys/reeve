import { useState } from 'react'
import { Link, createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { ArrowLeft, Plus, X } from 'lucide-react'
import { useList } from '@/api/endpoints/devices/devices'
import {
  getIndexQueryKey,
  useCreate,
} from '@/api/endpoints/join-tokens/join-tokens'
import type { CreatedJoinToken } from '@/api/model'
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
import { CopyButton } from '@/components/copy-button'
import { fmtUnix } from '@/lib/format'

export const Route = createFileRoute('/_app/enrollment/new')({
  component: JoinTokenCreatePage,
})

/**
 * Mint a join token. The raw token appears exactly once below — only its
 * hash is stored server-side.
 */
function JoinTokenCreatePage() {
  const qc = useQueryClient()
  const create = useCreate()
  const devices = useList()
  const deviceRows = devices.data?.status === 200 ? devices.data.data : []

  const [ttlHours, setTtlHours] = useState('24')
  const [maxUses, setMaxUses] = useState('1')
  const [deviceId, setDeviceId] = useState('')
  const [fleet, setFleet] = useState('')
  const [site, setSite] = useState('')
  const [type, setType] = useState('')
  const [tags, setTags] = useState<Record<string, string>>({})
  const [tagKey, setTagKey] = useState('')
  const [tagValue, setTagValue] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [minted, setMinted] = useState<CreatedJoinToken | null>(null)

  const distinct = (get: (d: (typeof deviceRows)[number]) => string | null | undefined) =>
    [...new Set(deviceRows.map(get).filter((v): v is string => !!v))].sort()

  const submit = async () => {
    setError(null)
    const res = await create.mutateAsync({
      data: {
        ttl_secs: ttlHours.trim() === '' ? null : Math.round(Number(ttlHours) * 3600),
        max_uses: maxUses.trim() === '' ? null : Number(maxUses),
        device_id: deviceId || null,
        fleet: fleet.trim() || null,
        site: site.trim() || null,
        type: type.trim() || null,
        tags: Object.keys(tags).length > 0 ? tags : null,
      },
    })
    if (res.status === 201) {
      setMinted(res.data)
      void qc.invalidateQueries({ queryKey: getIndexQueryKey() })
    } else {
      setError(`HTTP ${res.status}`)
    }
  }

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/enrollment">
            <ArrowLeft className="size-4" />
            Enrollment
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">New join token</h1>
      </div>

      {minted ? (
        <Card className="max-w-2xl border-emerald-500/40">
          <CardHeader>
            <CardTitle className="text-base">One-time token</CardTitle>
            <CardDescription>
              Shown exactly once — only the hash is stored. Copy it now and
              pass it to the device (reeve-agent enroll).
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-3">
            <div className="flex items-center gap-2">
              <code className="break-all rounded bg-muted px-3 py-2 font-mono text-sm">
                {minted.join_token}
              </code>
              <CopyButton value={minted.join_token} />
            </div>
            <p className="text-sm text-muted-foreground">
              Expires {fmtUnix(minted.expires_at)} · max {minted.max_uses} use
              {minted.max_uses === 1 ? '' : 's'}
              {minted.device_id ? ` · re-enrolls ${minted.device_id}` : ''}
            </p>
            {(fleet || site || type || Object.keys(tags).length > 0) && (
              <p className="text-sm text-muted-foreground">
                Pre-assigns:{' '}
                {[
                  fleet && `Fleet ${fleet}`,
                  site && `Site ${site}`,
                  type && `Type ${type}`,
                  ...Object.entries(tags).map(
                    ([k, v]) => `${k}${v ? `=${v}` : ''}`,
                  ),
                ]
                  .filter(Boolean)
                  .join(' · ')}
              </p>
            )}
            <div className="flex gap-2">
              <Button variant="outline" size="sm" asChild>
                <Link to="/enrollment">Back to tokens</Link>
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={() => setMinted(null)}
              >
                Mint another
              </Button>
            </div>
          </CardContent>
        </Card>
      ) : (
        <Card className="max-w-2xl">
          <CardHeader>
            <CardTitle className="text-base">Token parameters</CardTitle>
            <CardDescription>
              Defaults: 24 h TTL, single use, no device binding.
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-4">
            <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="ttl">TTL (hours)</Label>
                <Input
                  id="ttl"
                  type="number"
                  min={0}
                  step="any"
                  value={ttlHours}
                  onChange={(e) => setTtlHours(e.target.value)}
                />
              </div>
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="max-uses">Max uses</Label>
                <Input
                  id="max-uses"
                  type="number"
                  min={1}
                  value={maxUses}
                  onChange={(e) => setMaxUses(e.target.value)}
                />
              </div>
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="device-binding">
                Re-enroll binding (optional)
              </Label>
              <Select
                value={deviceId || '__none__'}
                onValueChange={(v) => setDeviceId(v === '__none__' ? '' : v)}
              >
                <SelectTrigger id="device-binding" className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="__none__">
                    None — enrolls a new device
                  </SelectItem>
                  {deviceRows.map((d) => (
                    <SelectItem key={d.deviceId} value={d.deviceId}>
                      {d.hostname} ({d.deviceId})
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <span className="text-xs text-muted-foreground">
                Bound tokens re-key an EXISTING device (lost identity,
                reinstall) instead of enrolling a new one.
              </span>
            </div>

            <div className="flex flex-col gap-3 rounded-md border p-3">
              <div className="flex flex-col gap-0.5">
                <Label>Pre-assign (optional)</Label>
                <span className="text-xs text-muted-foreground">
                  Where a device lands the moment it enrolls with this token.
                </span>
              </div>
              <div className="grid grid-cols-1 gap-4 md:grid-cols-3">
                {(
                  [
                    ['Fleet', fleet, setFleet, (d: (typeof deviceRows)[number]) => d.fleet],
                    ['Site', site, setSite, (d: (typeof deviceRows)[number]) => d.site],
                    ['Device type', type, setType, (d: (typeof deviceRows)[number]) => d.type],
                  ] as const
                ).map(([label, value, setter, get], i) => (
                  <div key={label} className="flex flex-col gap-1.5">
                    <Label htmlFor={`assign-${i}`}>{label}</Label>
                    <Input
                      id={`assign-${i}`}
                      list={`assign-${i}-options`}
                      value={value}
                      onChange={(e) => setter(e.target.value)}
                      placeholder="none"
                    />
                    <datalist id={`assign-${i}-options`}>
                      {distinct(get).map((o) => (
                        <option key={o} value={o} />
                      ))}
                    </datalist>
                  </div>
                ))}
              </div>
              <div className="flex flex-col gap-1.5">
                <Label>Tags</Label>
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
                <div className="flex items-center gap-2">
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
                    type="button"
                    variant="outline"
                    size="sm"
                    disabled={!tagKey.trim()}
                    onClick={() => {
                      const k = tagKey.trim()
                      if (k) setTags((prev) => ({ ...prev, [k]: tagValue.trim() }))
                      setTagKey('')
                      setTagValue('')
                    }}
                  >
                    <Plus className="size-4" />
                  </Button>
                </div>
              </div>
            </div>

            <div className="flex items-center gap-3">
              <Button onClick={() => void submit()} disabled={create.isPending}>
                {create.isPending ? 'Minting…' : 'Mint token'}
              </Button>
              {error && <span className="text-sm text-destructive">{error}</span>}
            </div>
          </CardContent>
        </Card>
      )}
    </div>
  )
}
