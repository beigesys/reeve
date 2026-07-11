import { Link, createFileRoute, useNavigate } from '@tanstack/react-router'
import { Plus } from 'lucide-react'
import { useListRollouts } from '@/api/endpoints/rollouts/rollouts'
import { Button } from '@/components/ui/button'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { RolloutStateBadge } from '@/components/rollout-badges'
import { fmtAgo } from '@/lib/format'
import { usePollInterval } from '@/lib/sse'

export const Route = createFileRoute('/_app/rollouts/')({
  component: RolloutsPage,
})

function RolloutsPage() {
  const refetchInterval = usePollInterval(30_000)
  const rollouts = useListRollouts({ query: { refetchInterval } })
  const navigate = useNavigate()
  const rows = rollouts.data?.status === 200 ? rollouts.data.data : []

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-xl font-semibold tracking-tight">Rollouts</h1>
        <Button variant="outline" size="sm" asChild>
          <Link to="/rollouts/new">
            <Plus className="size-4" />
            New rollout
          </Link>
        </Button>
      </div>

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Rollout</TableHead>
              <TableHead>State</TableHead>
              <TableHead>Scope</TableHead>
              <TableHead>Wave</TableHead>
              <TableHead>Devices</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Updated</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.length === 0 ? (
              <TableRow>
                <TableCell
                  colSpan={7}
                  className="h-16 text-center text-muted-foreground"
                >
                  {rollouts.isLoading ? 'Loading…' : 'No rollouts yet.'}
                </TableCell>
              </TableRow>
            ) : (
              rows.map((r) => (
                <TableRow
                  key={r.rolloutId}
                  className="cursor-pointer"
                  onClick={() =>
                    navigate({
                      to: '/rollouts/$rollout-id',
                      params: { 'rollout-id': r.rolloutId },
                    })
                  }
                >
                  <TableCell>
                    <span className="flex flex-col">
                      <span className="font-mono text-sm">{r.rolloutId}</span>
                      <span className="text-xs text-muted-foreground">
                        by {r.createdBy}
                        {r.pauseReason ? ` — paused: ${r.pauseReason}` : ''}
                      </span>
                    </span>
                  </TableCell>
                  <TableCell>
                    <RolloutStateBadge state={r.state} />
                  </TableCell>
                  <TableCell className="max-w-72 truncate text-sm">
                    {r.scopeDescription}
                  </TableCell>
                  <TableCell className="text-sm">
                    {r.currentWave + 1} / {r.waveCount}
                  </TableCell>
                  <TableCell className="text-sm">{r.deviceCount}</TableCell>
                  <TableCell className="text-sm text-muted-foreground">
                    {fmtAgo(r.createdAt)} ago
                  </TableCell>
                  <TableCell className="text-sm text-muted-foreground">
                    {fmtAgo(r.updatedAt)} ago
                  </TableCell>
                </TableRow>
              ))
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  )
}
