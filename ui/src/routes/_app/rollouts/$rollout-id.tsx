import { type ReactNode } from 'react'
import { Link, createFileRoute, useNavigate } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { ArrowLeft } from 'lucide-react'
import { useMe } from '@/api/endpoints/auth/auth'
import {
  getListRolloutsQueryKey,
  getRolloutStatusQueryKey,
  useAbortRoute,
  usePauseRoute,
  useResumeRoute,
  useRollbackRoute,
  useRolloutStatus,
} from '@/api/endpoints/rollouts/rollouts'
import type { WaveStatus } from '@/api/model'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { ConfirmButton } from '@/components/confirm-button'
import {
  DeviceClassBadge,
  RolloutStateBadge,
  WaveStateBadge,
} from '@/components/rollout-badges'
import { fmtUnix } from '@/lib/format'
import { usePollInterval } from '@/lib/sse'

export const Route = createFileRoute('/_app/rollouts/$rollout-id')({
  component: RolloutDetailPage,
})

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="text-sm">{children}</span>
    </div>
  )
}

function WaveCard({ wave }: { wave: WaveStatus }) {
  const gateEntries = Object.entries(wave.gate)
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          Wave {wave.index + 1}
          <WaveStateBadge state={wave.state} />
        </CardTitle>
        <CardDescription>
          {wave.counts.converged}/{wave.counts.total} converged ·{' '}
          {wave.counts.pending} pending · {wave.counts.failed} failed ·{' '}
          {wave.counts.undetermined} undetermined · {wave.counts.unaffected}{' '}
          unaffected
          {wave.soakStartedAt != null && ` — soak started ${fmtUnix(wave.soakStartedAt)}`}
          {wave.gatedAt != null && ` — gated ${fmtUnix(wave.gatedAt)}`}
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        {gateEntries.length > 0 && (
          <div>
            <h4 className="mb-1 text-xs font-medium text-muted-foreground">
              Gate evaluation
            </h4>
            <pre className="overflow-x-auto rounded bg-muted p-2 font-mono text-xs">
              {JSON.stringify(wave.gate, null, 2)}
            </pre>
          </div>
        )}
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Device</TableHead>
              <TableHead>Status</TableHead>
              <TableHead>Advanced</TableHead>
              <TableHead>Notes</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {wave.devices.map((d) => (
              <TableRow key={d.deviceId}>
                <TableCell>
                  <Link
                    to="/devices/$device-id"
                    params={{ 'device-id': d.deviceId }}
                    className="font-mono text-xs underline-offset-4 hover:underline"
                  >
                    {d.deviceId}
                  </Link>
                </TableCell>
                <TableCell>
                  <DeviceClassBadge state={d.status} />
                </TableCell>
                <TableCell className="text-sm text-muted-foreground">
                  {d.advanced ? fmtUnix(d.advancedAt) : 'not yet'}
                </TableCell>
                <TableCell>
                  {d.unaffected && (
                    <Badge variant="outline" className="font-normal text-muted-foreground">
                      unaffected
                    </Badge>
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  )
}

function RolloutDetailPage() {
  const params = Route.useParams()
  const rolloutId = params['rollout-id']
  const qc = useQueryClient()
  const navigate = useNavigate()
  const refetchInterval = usePollInterval(10_000)
  const status = useRolloutStatus(rolloutId, { query: { refetchInterval } })
  const me = useMe()

  const pause = usePauseRoute()
  const resume = useResumeRoute()
  const abort = useAbortRoute()
  const rollback = useRollbackRoute()

  const rollout = status.data?.status === 200 ? status.data.data : undefined
  const role = me.data?.status === 200 ? me.data.data.effectiveRole : undefined
  const operator = role === 'admin' || role === 'operator'

  const invalidate = () => {
    void qc.invalidateQueries({ queryKey: getRolloutStatusQueryKey(rolloutId) })
    void qc.invalidateQueries({ queryKey: getListRolloutsQueryKey() })
  }
  const acting =
    pause.isPending || resume.isPending || abort.isPending || rollback.isPending

  const doRollback = async () => {
    const res = await rollback.mutateAsync({ rolloutId })
    if (res.status === 201) {
      invalidate()
      void navigate({
        to: '/rollouts/$rollout-id',
        params: { 'rollout-id': res.data.rolloutId },
      })
    }
  }

  const cohortDescription =
    rollout && typeof rollout.cohort.description === 'string'
      ? rollout.cohort.description
      : (rollout?.scopeDescription ?? '')

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/rollouts">
            <ArrowLeft className="size-4" />
            Rollouts
          </Link>
        </Button>
        <h1 className="font-mono text-xl font-semibold tracking-tight">
          {rolloutId}
        </h1>
        {rollout && <RolloutStateBadge state={rollout.state} />}
        {rollout && operator && (
          <span className="ml-auto flex items-center gap-2">
            {rollout.state === 'active' && (
              <Button
                variant="outline"
                size="sm"
                disabled={acting}
                onClick={() =>
                  pause.mutate({ rolloutId }, { onSuccess: invalidate })
                }
              >
                Pause
              </Button>
            )}
            {rollout.state === 'paused' && (
              <Button
                variant="outline"
                size="sm"
                disabled={acting}
                onClick={() =>
                  resume.mutate({ rolloutId }, { onSuccess: invalidate })
                }
              >
                Resume
              </Button>
            )}
            {(rollout.state === 'active' || rollout.state === 'paused') && (
              <ConfirmButton
                label="Abort"
                confirmLabel="Abort rollout?"
                description="Stops advancing devices. Devices already updated keep the new config."
                disabled={acting}
                onConfirm={() =>
                  abort.mutate({ rolloutId }, { onSuccess: invalidate })
                }
              />
            )}
            <ConfirmButton
              label="Undo this rollout"
              confirmLabel="Undo this rollout?"
              description="Returns the affected devices to the configuration they had before this rollout, in a new single-wave rollout."
              disabled={acting}
              onConfirm={() => void doRollback()}
            />
            {rollback.data && rollback.data.status !== 201 && (
              <span className="text-sm text-destructive">
                Nothing to undo.
              </span>
            )}
          </span>
        )}
      </div>

      {status.data && status.data.status === 404 ? (
        <p className="text-sm text-destructive">Unknown rollout.</p>
      ) : !rollout ? (
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : (
        <>
          {rollout.pauseReason && (
            <p className="rounded-md border border-amber-500/40 p-3 text-sm text-amber-600 dark:text-amber-400">
              Paused: {rollout.pauseReason}
            </p>
          )}

          <Card>
            <CardHeader>
              <CardTitle className="text-base">Rollout</CardTitle>
            </CardHeader>
            <CardContent className="grid grid-cols-2 gap-4 md:grid-cols-4">
              <Field label="Rolling out to">{rollout.scopeDescription}</Field>
              <Field label="Created">
                {fmtUnix(rollout.createdAt)} by {rollout.createdBy}
              </Field>
              <Field label="Current wave">
                {rollout.currentWave + 1} / {rollout.waves.length}
              </Field>
              <Field label="Pinned unaffected">
                {rollout.pinnedUnaffected}
              </Field>
              <Field label="Gate policy">
                soak {rollout.gate.soakSecs}s · pass{' '}
                {rollout.gate.passFraction} · timeout{' '}
                {rollout.gate.gateTimeoutSecs}s
              </Field>
              <Field label="Undetermined allowance">
                {rollout.gate.undeterminedAllowance ?? 'unlimited'}
              </Field>
              <Field label="Failure threshold">{rollout.failureThreshold}</Field>
              <Field label="Cohort">{cohortDescription}</Field>
            </CardContent>
          </Card>

          {rollout.waves.map((wave) => (
            <WaveCard key={wave.index} wave={wave} />
          ))}

          <Card>
            <CardHeader>
              <CardTitle className="text-base">Transitions</CardTitle>
              <CardDescription>Audited lifecycle actions.</CardDescription>
            </CardHeader>
            <CardContent className="flex flex-col divide-y">
              {rollout.transitions.length === 0 ? (
                <p className="text-sm text-muted-foreground">None.</p>
              ) : (
                rollout.transitions.map((t) => (
                  <div key={t.seq} className="flex items-center gap-2 py-2 text-sm">
                    <Badge variant="outline" className="font-normal">
                      {t.action}
                    </Badge>
                    <span>{t.detail ?? ''}</span>
                    <span className="ml-auto text-xs text-muted-foreground">
                      {t.author}, {fmtUnix(t.ts)}
                    </span>
                  </div>
                ))
              )}
            </CardContent>
          </Card>
        </>
      )}
    </div>
  )
}
