import { Link, createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import type { ReactNode } from 'react'
import { Plus } from 'lucide-react'
import { useMe } from '@/api/endpoints/auth/auth'
import { useDurabilityStatus } from '@/api/endpoints/durability/durability'
import { useServerInfo } from '@/api/endpoints/ops/ops'
import {
  getListTokensRouteQueryKey,
  useFederationStatus,
  useListTokensRoute,
  useRevokeTokenRoute,
} from '@/api/endpoints/federation/federation'
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
import { cn } from '@/lib/utils'
import { fmtUnix } from '@/lib/format'
import { usePollInterval } from '@/lib/sse'

export const Route = createFileRoute('/_app/ops/')({
  component: OpsPage,
})

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="text-sm">{children}</span>
    </div>
  )
}

/** Durability posture: snapshot/changeset shipping and verified restore. */
function DurabilityCard() {
  const refetchInterval = usePollInterval(30_000)
  const status = useDurabilityStatus({ query: { refetchInterval } })
  const d = status.data?.status === 200 ? status.data.data : undefined
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          Durability
          {d && (
            <Badge
              variant="outline"
              className={cn(
                'font-normal',
                d.degraded
                  ? 'border-red-500/40 text-red-600 dark:text-red-400'
                  : 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400',
              )}
            >
              {d.degraded ? 'degraded' : 'healthy'}
            </Badge>
          )}
        </CardTitle>
        <CardDescription>
          Snapshot and changeset shipping with verified restore. A tier whose
          restore has never been verified reads as none.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {!d ? (
          <p className="text-sm text-muted-foreground">
            {status.isLoading ? 'Loading…' : 'Unavailable.'}
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-4 md:grid-cols-4">
            <Field label="Configured tier">{d.tier}</Field>
            <Field label="Effective tier">{d.effective_tier}</Field>
            <Field label="Server epoch">
              <span className="font-mono">{d.epoch}</span>
            </Field>
            <Field label="Generation">
              <span className="font-mono text-xs">{d.generation ?? '—'}</span>
            </Field>
            <Field label="Pending changesets">{d.pending_changesets}</Field>
            <Field label="Last snapshot">
              {fmtUnix(d.last_snapshot_at)}
              {d.snapshot_age_secs != null &&
                ` (${Math.round(d.snapshot_age_secs / 60)} min ago)`}
            </Field>
            <Field label="Last changeset">
              {fmtUnix(d.last_changeset_at)}
              {d.last_changeset_seq != null && ` (seq ${d.last_changeset_seq})`}
            </Field>
            <Field label="Last verified restore">
              {d.last_verify
                ? `${d.last_verify.outcome} at ${fmtUnix(d.last_verify.finished_at)}`
                : 'never'}
            </Field>
            <Field label="Last error">
              {d.last_error ?? '—'}
            </Field>
          </div>
        )}
      </CardContent>
    </Card>
  )
}

/** Federation posture for this tier. */
function FederationCard() {
  const refetchInterval = usePollInterval(30_000)
  const status = useFederationStatus({ query: { refetchInterval } })
  const f = status.data?.status === 200 ? status.data.data : undefined
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          Federation
          {f && (
            <Badge variant="secondary" className="font-normal">
              {f.mode}
            </Badge>
          )}
        </CardTitle>
        <CardDescription>
          This tier's federation state.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {!f ? (
          <p className="text-sm text-muted-foreground">
            {status.isLoading ? 'Loading…' : 'Unavailable.'}
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-4 md:grid-cols-4">
            <Field label="Mode">{f.mode}</Field>
            <Field label="Child tiers">{f.childTiers}</Field>
            <Field label="Upstream">{f.upstream ?? '—'}</Field>
            <Field label="Site">{f.site ?? '—'}</Field>
            <Field label="Upstream origin head">
              {f.upstreamOriginHead ?? '—'}
            </Field>
            <Field label="Last sync">{fmtUnix(f.lastSyncAt)}</Field>
            <Field label="Sync interval">
              {f.syncIntervalSecs != null ? `${f.syncIntervalSecs}s` : '—'}
            </Field>
            <Field label="Last sync error">{f.lastSyncError ?? '—'}</Field>
          </div>
        )}
      </CardContent>
    </Card>
  )
}

/** Server version + advertised extensions (capability advertisement). */
function CapabilitiesCard() {
  const caps = useServerInfo({ query: { staleTime: 60_000 } })
  const c = caps.data?.status === 200 ? caps.data.data : undefined
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Server</CardTitle>
        <CardDescription>
          Version and compiled-in extensions (capability advertisement —
          the server cannot advertise what it does not contain).
        </CardDescription>
      </CardHeader>
      <CardContent>
        {!c ? (
          <p className="text-sm text-muted-foreground">
            {caps.isLoading ? 'Loading…' : 'Unavailable.'}
          </p>
        ) : (
          <div className="flex flex-col gap-3">
            <Field label="This server">
              <span className="flex flex-wrap items-center gap-2">
                <Badge
                  variant="outline"
                  className={cn(
                    'font-normal',
                    c.tier === 'site'
                      ? 'border-blue-500/40 text-blue-600 dark:text-blue-400'
                      : 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400',
                  )}
                >
                  {c.tier === 'site' ? 'Site gateway' : 'Root (hub)'}
                </Badge>
                {c.tier === 'site' && c.site && (
                  <span className="text-sm text-muted-foreground">
                    serving {c.site}
                    {c.upstream ? ' · reports to a hub' : ''}
                  </span>
                )}
              </span>
            </Field>
            <Field label="Version">
              <span className="font-mono">{c.serverVersion}</span>
            </Field>
            <Field label="Extensions">
              {(c.extensions ?? []).length === 0 ? (
                'none (core-only build)'
              ) : (
                <span className="flex flex-wrap gap-1">
                  {(c.extensions ?? []).map((e) => (
                    <Badge key={e} variant="secondary" className="font-mono font-normal">
                      {e}
                    </Badge>
                  ))}
                </span>
              )}
            </Field>
          </div>
        )}
      </CardContent>
    </Card>
  )
}

/** Tier tokens — admin only. */
function TierTokensCard() {
  const qc = useQueryClient()
  const refetchInterval = usePollInterval(30_000)
  const tokens = useListTokensRoute({ query: { refetchInterval } })
  const revoke = useRevokeTokenRoute()
  const rows =
    tokens.data?.status === 200 ? tokens.data.data.tierTokens : []

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center justify-between gap-2 text-base">
          Tier tokens
          <Button variant="outline" size="sm" asChild>
            <Link to="/ops/tier-tokens/new">
              <Plus className="size-4" />
              New tier token
            </Link>
          </Button>
        </CardTitle>
        <CardDescription>
          Child-tier sync credentials. Raw tokens are shown once at
          creation; revoke to disable one.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Site</TableHead>
              <TableHead>Sync prefixes</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Expires</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.length === 0 ? (
              <TableRow>
                <TableCell
                  colSpan={6}
                  className="h-16 text-center text-muted-foreground"
                >
                  {tokens.isLoading ? 'Loading…' : 'No tier tokens.'}
                </TableCell>
              </TableRow>
            ) : (
              rows.map((t) => (
                <TableRow key={t.name}>
                  <TableCell className="font-mono text-sm">{t.name}</TableCell>
                  <TableCell className="font-mono text-sm">{t.site}</TableCell>
                  <TableCell className="font-mono text-xs text-muted-foreground">
                    {t.syncPrefixes.join(', ')}
                  </TableCell>
                  <TableCell className="text-sm text-muted-foreground">
                    {fmtUnix(t.createdAt)} by {t.createdBy}
                  </TableCell>
                  <TableCell className="text-sm text-muted-foreground">
                    {t.revokedAt != null
                      ? `revoked ${fmtUnix(t.revokedAt)}`
                      : t.expiresAt != null
                        ? fmtUnix(t.expiresAt)
                        : 'never'}
                  </TableCell>
                  <TableCell className="text-right">
                    {t.revokedAt == null && (
                      <ConfirmButton
                        label="Revoke"
                        confirmLabel="Really revoke?"
                        disabled={revoke.isPending}
                        onConfirm={() =>
                          revoke.mutate(
                            { name: t.name },
                            {
                              onSuccess: () =>
                                void qc.invalidateQueries({
                                  queryKey: getListTokensRouteQueryKey(),
                                }),
                            },
                          )
                        }
                      />
                    )}
                  </TableCell>
                </TableRow>
              ))
            )}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  )
}

function OpsPage() {
  const me = useMe()
  const role = me.data?.status === 200 ? me.data.data.effectiveRole : undefined

  return (
    <div className="flex flex-col gap-4 p-6">
      <h1 className="text-xl font-semibold tracking-tight">Ops</h1>
      <DurabilityCard />
      <FederationCard />
      {role === 'admin' && <TierTokensCard />}
      <CapabilitiesCard />
    </div>
  )
}
