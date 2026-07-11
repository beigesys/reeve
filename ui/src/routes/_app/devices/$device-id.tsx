import { useState, type ReactNode } from 'react'
import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowLeft } from 'lucide-react'
import { useMe } from '@/api/endpoints/auth/auth'
import { useDetail, useJournal } from '@/api/endpoints/devices/devices'
import type { DeviceDetail, JournalEntry, RenderProvenance } from '@/api/model'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { DeploymentStateBadge } from '@/components/deployment-state-badge'
import { DeviceTerminal } from '@/components/device-terminal'
import { PresenceBadge } from '@/components/presence-badge'
import { fmtDigest, fmtRfc3339, fmtUnix } from '@/lib/format'
import { usePollInterval } from '@/lib/sse'

export const Route = createFileRoute('/_app/devices/$device-id')({
  component: DeviceDetailPage,
})

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="text-sm">{children}</span>
    </div>
  )
}

function Mono({ children }: { children: ReactNode }) {
  return <span className="font-mono text-xs">{children}</span>
}

function OverviewTab({ device }: { device: DeviceDetail }) {
  return (
    <div className="flex flex-col gap-4">
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Metadata</CardTitle>
        </CardHeader>
        <CardContent className="grid grid-cols-2 gap-4 md:grid-cols-4">
          <Field label="Device id">
            <Mono>{device.deviceId}</Mono>
          </Field>
          <Field label="Architecture">{device.arch}</Field>
          <Field label="Agent version">{device.agentVersion}</Field>
          <Field label="Enrolled">{fmtUnix(device.enrolledAt)}</Field>
          <Field label="Class">{device.class ?? '—'}</Field>
          <Field label="Region">{device.region ?? '—'}</Field>
          <Field label="Site">{device.site ?? '—'}</Field>
          <Field label="Tier origin">
            {device.tierOrigin ?? 'enrolled here'}
          </Field>
          <Field label="Last seen">{fmtUnix(device.lastSeenAt)}</Field>
          <Field label="Labels">
            {Object.keys(device.labels).length === 0 ? (
              '—'
            ) : (
              <span className="flex flex-wrap gap-1">
                {Object.entries(device.labels).map(([k, v]) => (
                  <Badge
                    key={k}
                    variant="secondary"
                    className="font-mono text-xs font-normal"
                  >
                    {k}={String(v)}
                  </Badge>
                ))}
              </span>
            )}
          </Field>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Deployments</CardTitle>
          <CardDescription>
            Current per-deployment state as last reported by the device.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {device.deployments.length === 0 ? (
            <p className="text-sm text-muted-foreground">No deployments.</p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Deployment id</TableHead>
                  <TableHead>State</TableHead>
                  <TableHead>Observed</TableHead>
                  <TableHead>Received</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {device.deployments.map((d) => (
                  <TableRow key={d.deploymentId}>
                    <TableCell>
                      <Mono>{d.deploymentId}</Mono>
                    </TableCell>
                    <TableCell>
                      <DeploymentStateBadge state={d.state} />
                    </TableCell>
                    <TableCell className="text-sm text-muted-foreground">
                      {fmtRfc3339(d.observedAt)}
                    </TableCell>
                    <TableCell className="text-sm text-muted-foreground">
                      {fmtUnix(d.receivedAt)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>

      <RenderProvenanceCard render={device.render ?? null} />
    </div>
  )
}

/**
 * Render provenance (docs/decisions/delivery.md D13): which tree
 * revision this device's State Manifest was rendered from, the packed
 * manifestVersion decoded to (epoch, counter), the bundle digest the
 * agent pulls, and each app's secrets_version.
 */
function RenderProvenanceCard({ render }: { render: RenderProvenance | null }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Render provenance</CardTitle>
        <CardDescription>
          How this device's current State Manifest was produced.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {!render ? (
          <p className="text-sm text-muted-foreground">
            Not rendered yet — no State Manifest exists for this device.
          </p>
        ) : (
          <div className="flex flex-col gap-4">
            <div className="grid grid-cols-2 gap-4 md:grid-cols-4">
              <Field label="Rendered revision (local stream)">
                {render.renderedRevision}
              </Field>
              <Field label="Manifest version (packed u64)">
                <Mono>{render.manifestVersion}</Mono>
              </Field>
              <Field label="Epoch / counter (decoded)">
                {render.epoch} / {render.counter}
              </Field>
              <Field label="Render generation">{render.generation}</Field>
              <Field label="Content digest">
                <Mono>{fmtDigest(render.contentDigest)}</Mono>
              </Field>
              <Field label="Bundle digest">
                <Mono>{fmtDigest(render.bundleDigest)}</Mono>
              </Field>
              <Field label="ETag">
                <Mono>{render.etag}</Mono>
              </Field>
              <Field label="Updated">{fmtUnix(render.updatedAt)}</Field>
            </div>
            {render.apps.length > 0 && (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>App</TableHead>
                    <TableHead>Deployment id</TableHead>
                    <TableHead>Secrets version</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {render.apps.map((a) => (
                    <TableRow key={a.appId}>
                      <TableCell>{a.appId}</TableCell>
                      <TableCell>
                        <Mono>{a.deploymentId ?? '—'}</Mono>
                      </TableCell>
                      <TableCell>
                        <Mono>{a.secrets_version ?? '—'}</Mono>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  )
}

function JournalRecordRow({ record }: { record: JournalEntry }) {
  return (
    <div className="flex flex-col gap-1 border-b px-4 py-3 last:border-b-0">
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        <Badge variant="outline" className="font-normal">
          {record.kind}
        </Badge>
        <span>seq {record.seq}</span>
        <span>observed {fmtRfc3339(record.observedAt)}</span>
        <span>received {fmtUnix(record.receivedAt)}</span>
      </div>
      {record.payload != null && (
        <pre className="overflow-x-auto rounded bg-muted p-2 font-mono text-xs">
          {JSON.stringify(record.payload, null, 2)}
        </pre>
      )}
    </div>
  )
}

const JOURNAL_PAGE_SIZE = 50

/** One fetched page; the last page offers "load older" via nextBeforeSeq. */
function JournalPageBlock({
  deviceId,
  beforeSeq,
  isLast,
  onLoadOlder,
}: {
  deviceId: string
  beforeSeq: number | undefined
  isLast: boolean
  onLoadOlder: (nextBeforeSeq: number) => void
}) {
  const refetchInterval = usePollInterval(10_000)
  const page = useJournal(
    deviceId,
    { limit: JOURNAL_PAGE_SIZE, before_seq: beforeSeq },
    // Only the newest page live-updates; older pages are immutable.
    { query: { refetchInterval: beforeSeq == null ? refetchInterval : false } },
  )

  if (page.isLoading)
    return <p className="px-4 py-3 text-sm text-muted-foreground">Loading…</p>
  if (!page.data || page.data.status !== 200)
    return (
      <p className="px-4 py-3 text-sm text-destructive">
        Could not load journal page.
      </p>
    )
  const { records, nextBeforeSeq } = page.data.data
  return (
    <>
      {records.length === 0 && beforeSeq == null ? (
        <p className="px-4 py-3 text-sm text-muted-foreground">
          The journal is empty.
        </p>
      ) : (
        records.map((r) => <JournalRecordRow key={r.seq} record={r} />)
      )}
      {isLast && nextBeforeSeq != null && (
        <div className="p-3">
          <Button
            variant="outline"
            size="sm"
            onClick={() => onLoadOlder(nextBeforeSeq)}
          >
            Load older records
          </Button>
        </div>
      )}
    </>
  )
}

function JournalTab({ deviceId }: { deviceId: string }) {
  // Newest page first; each "load older" pins another page by its
  // before_seq cursor (server pages are stable snapshots by seq).
  const [olderCursors, setOlderCursors] = useState<number[]>([])
  const cursors: (number | undefined)[] = [undefined, ...olderCursors]
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Status journal</CardTitle>
        <CardDescription>
          Newest first (spec/reeve/05-health-journal.md §7.3).
        </CardDescription>
      </CardHeader>
      <CardContent className="p-0">
        {cursors.map((cursor, i) => (
          <JournalPageBlock
            key={cursor ?? 'head'}
            deviceId={deviceId}
            beforeSeq={cursor}
            isLast={i === cursors.length - 1}
            onLoadOlder={(next) => setOlderCursors((prev) => [...prev, next])}
          />
        ))}
      </CardContent>
    </Card>
  )
}

function DeviceDetailPage() {
  const params = Route.useParams()
  const deviceId = params['device-id']
  const refetchInterval = usePollInterval(10_000)
  const detail = useDetail(deviceId, { query: { refetchInterval } })
  const me = useMe()

  const device = detail.data?.status === 200 ? detail.data.data : undefined
  const role = me.data?.status === 200 ? me.data.data.effectiveRole : undefined
  const operator = role === 'admin' || role === 'operator'

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/devices">
            <ArrowLeft className="size-4" />
            Devices
          </Link>
        </Button>
        {device && (
          <>
            <h1 className="text-xl font-semibold tracking-tight">
              {device.hostname}
            </h1>
            <PresenceBadge presence={device.presence} />
            {device.stale && (
              <Badge variant="outline" className="font-normal text-muted-foreground">
                stale identity
              </Badge>
            )}
          </>
        )}
      </div>

      {detail.data && detail.data.status === 404 ? (
        <p className="text-sm text-destructive">Unknown device.</p>
      ) : !device ? (
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : (
        <Tabs defaultValue="overview">
          <TabsList>
            <TabsTrigger value="overview">Overview</TabsTrigger>
            <TabsTrigger value="journal">Journal</TabsTrigger>
            <TabsTrigger value="terminal">Terminal</TabsTrigger>
          </TabsList>
          <TabsContent value="overview">
            <OverviewTab device={device} />
          </TabsContent>
          <TabsContent value="journal">
            <JournalTab deviceId={deviceId} />
          </TabsContent>
          <TabsContent value="terminal">
            <Card>
              <CardHeader>
                <CardTitle className="text-base">Remote terminal</CardTitle>
                <CardDescription>
                  Disabled by default; enabled only via desired state (a tree
                  revision). Sessions are short-lived and audited
                  (spec/reeve/03-terminal.md §5).
                </CardDescription>
              </CardHeader>
              <CardContent>
                <DeviceTerminal
                  deviceId={deviceId}
                  online={device.presence.state === 'online'}
                  operator={operator}
                />
              </CardContent>
            </Card>
          </TabsContent>
        </Tabs>
      )}
    </div>
  )
}
