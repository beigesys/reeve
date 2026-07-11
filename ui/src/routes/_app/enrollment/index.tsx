import { Link, createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Plus } from 'lucide-react'
import {
  getIndexQueryKey,
  useDelete,
  useIndex,
} from '@/api/endpoints/join-tokens/join-tokens'
import type { JoinTokenInfo } from '@/api/model'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { ConfirmButton } from '@/components/confirm-button'
import { fmtUnix } from '@/lib/format'
import { usePollInterval } from '@/lib/sse'

export const Route = createFileRoute('/_app/enrollment/')({
  component: EnrollmentPage,
})

function tokenState(t: JoinTokenInfo): { label: string; cls: string } {
  const now = Math.floor(Date.now() / 1000)
  if (t.revoked_at != null)
    return { label: 'revoked', cls: 'border-red-500/40 text-red-600 dark:text-red-400' }
  if (t.expires_at <= now)
    return { label: 'expired', cls: 'text-muted-foreground' }
  if (t.uses >= t.max_uses)
    return { label: 'used up', cls: 'text-muted-foreground' }
  return {
    label: 'active',
    cls: 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400',
  }
}

/**
 * Join tokens (docs/decisions/auth.md D4): single-use by default,
 * TTL'd, listed by hash only — the raw token is shown exactly once at
 * creation.
 */
function EnrollmentPage() {
  const qc = useQueryClient()
  const refetchInterval = usePollInterval(30_000)
  const tokens = useIndex({ query: { refetchInterval } })
  const revoke = useDelete()
  const rows = tokens.data?.status === 200 ? tokens.data.data : []

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-xl font-semibold tracking-tight">Enrollment</h1>
        <Button variant="outline" size="sm" asChild>
          <Link to="/enrollment/new">
            <Plus className="size-4" />
            New join token
          </Link>
        </Button>
      </div>

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Token (hash)</TableHead>
              <TableHead>State</TableHead>
              <TableHead>Uses</TableHead>
              <TableHead>Expires</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Binding</TableHead>
              <TableHead className="text-right">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.length === 0 ? (
              <TableRow>
                <TableCell
                  colSpan={7}
                  className="h-16 text-center text-muted-foreground"
                >
                  {tokens.isLoading ? 'Loading…' : 'No join tokens.'}
                </TableCell>
              </TableRow>
            ) : (
              rows.map((t) => {
                const state = tokenState(t)
                return (
                  <TableRow key={t.token_hash}>
                    <TableCell className="font-mono text-xs">
                      {t.token_hash.slice(0, 16)}…
                    </TableCell>
                    <TableCell>
                      <Badge variant="outline" className={`font-normal ${state.cls}`}>
                        {state.label}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-sm">
                      {t.uses} / {t.max_uses}
                    </TableCell>
                    <TableCell className="text-sm text-muted-foreground">
                      {fmtUnix(t.expires_at)}
                    </TableCell>
                    <TableCell className="text-sm text-muted-foreground">
                      {fmtUnix(t.created_at)} by {t.created_by}
                    </TableCell>
                    <TableCell>
                      {t.device_id ? (
                        <Link
                          to="/devices/$device-id"
                          params={{ 'device-id': t.device_id }}
                          className="font-mono text-xs underline-offset-4 hover:underline"
                        >
                          re-enroll {t.device_id}
                        </Link>
                      ) : (
                        <span className="text-sm text-muted-foreground">new device</span>
                      )}
                    </TableCell>
                    <TableCell className="text-right">
                      {state.label === 'active' && (
                        <ConfirmButton
                          label="Revoke"
                          confirmLabel="Really revoke?"
                          disabled={revoke.isPending}
                          onConfirm={() =>
                            revoke.mutate(
                              { tokenHash: t.token_hash },
                              {
                                onSuccess: () =>
                                  void qc.invalidateQueries({
                                    queryKey: getIndexQueryKey(),
                                  }),
                              },
                            )
                          }
                        />
                      )}
                    </TableCell>
                  </TableRow>
                )
              })
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  )
}
