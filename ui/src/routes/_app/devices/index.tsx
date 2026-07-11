import { useMemo, useState } from 'react'
import { createFileRoute, useNavigate } from '@tanstack/react-router'
import {
  createColumnHelper,
  flexRender,
  getCoreRowModel,
  getFilteredRowModel,
  getSortedRowModel,
  useReactTable,
} from '@tanstack/react-table'
import { useList } from '@/api/endpoints/devices/devices'
import type { DeviceSummary } from '@/api/model'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { DeploymentStateBadge } from '@/components/deployment-state-badge'
import { PresenceBadge } from '@/components/presence-badge'
import { fmtAgo } from '@/lib/format'
import { usePollInterval } from '@/lib/sse'

export const Route = createFileRoute('/_app/devices/')({
  component: DevicesPage,
})

const columnHelper = createColumnHelper<DeviceSummary>()

/** Rollup of a device's per-deployment states, e.g. "3 installed · 1 failed". */
function DeploymentsRollup({ device }: { device: DeviceSummary }) {
  const counts = new Map<string, number>()
  for (const d of device.deployments) {
    counts.set(d.state, (counts.get(d.state) ?? 0) + 1)
  }
  if (counts.size === 0)
    return <span className="text-muted-foreground">no deployments</span>
  return (
    <span className="flex flex-wrap gap-1">
      {[...counts.entries()].map(([state, n]) => (
        <span key={state} className="flex items-center gap-1">
          <DeploymentStateBadge state={state} />
          {n > 1 && <span className="text-xs text-muted-foreground">×{n}</span>}
        </span>
      ))}
    </span>
  )
}

const columns = [
  columnHelper.accessor('hostname', {
    header: 'Device',
    cell: (info) => (
      <span className="flex flex-col">
        <span className="font-medium">
          {info.getValue()}
          {info.row.original.stale && (
            <Badge variant="outline" className="ml-2 font-normal text-muted-foreground">
              stale
            </Badge>
          )}
        </span>
        <span className="font-mono text-xs text-muted-foreground">
          {info.row.original.deviceId}
        </span>
      </span>
    ),
  }),
  columnHelper.accessor((d) => d.presence.state, {
    id: 'presence',
    header: 'Presence',
    cell: (info) => <PresenceBadge presence={info.row.original.presence} />,
  }),
  columnHelper.display({
    id: 'deployments',
    header: 'Deployments',
    cell: (info) => <DeploymentsRollup device={info.row.original} />,
  }),
  columnHelper.display({
    id: 'placement',
    header: 'Class / Region / Site',
    cell: (info) => {
      const d = info.row.original
      const parts = [d.class, d.region, d.site]
      return (
        <span className="text-sm">
          {parts.every((p) => !p)
            ? '—'
            : parts.map((p) => p ?? '·').join(' / ')}
        </span>
      )
    },
  }),
  columnHelper.display({
    id: 'labels',
    header: 'Labels',
    cell: (info) => {
      const labels = Object.entries(info.row.original.labels)
      if (labels.length === 0) return <span className="text-muted-foreground">—</span>
      return (
        <span className="flex max-w-64 flex-wrap gap-1">
          {labels.map(([k, v]) => (
            <Badge key={k} variant="secondary" className="font-mono text-xs font-normal">
              {k}={String(v)}
            </Badge>
          ))}
        </span>
      )
    },
  }),
  columnHelper.accessor('lastSeenAt', {
    header: 'Last seen',
    cell: (info) => {
      const v = info.getValue()
      return (
        <span className="text-sm text-muted-foreground">
          {v == null ? 'never' : `${fmtAgo(v)} ago`}
        </span>
      )
    },
  }),
]

function DevicesPage() {
  const refetchInterval = usePollInterval(30_000)
  const devices = useList({ query: { refetchInterval } })
  const navigate = useNavigate()
  const [filter, setFilter] = useState('')

  const rows: DeviceSummary[] = useMemo(
    () => (devices.data?.status === 200 ? devices.data.data : []),
    [devices.data],
  )

  const table = useReactTable({
    data: rows,
    columns,
    state: { globalFilter: filter },
    onGlobalFilterChange: setFilter,
    // Search across hostname, device id, placement and labels.
    globalFilterFn: (row, _columnId, value: string) => {
      const d = row.original
      const needle = value.toLowerCase()
      const hay = [
        d.hostname,
        d.deviceId,
        d.class,
        d.region,
        d.site,
        d.presence.state,
        ...Object.entries(d.labels).map(([k, v]) => `${k}=${String(v)}`),
        ...d.deployments.map((dep) => dep.state),
      ]
        .filter(Boolean)
        .join(' ')
        .toLowerCase()
      return hay.includes(needle)
    },
    getCoreRowModel: getCoreRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
    getSortedRowModel: getSortedRowModel(),
  })

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-xl font-semibold tracking-tight">Devices</h1>
        <Input
          placeholder="Filter by hostname, id, label, site…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="max-w-72"
        />
      </div>
      {devices.data && devices.data.status !== 200 ? (
        <p className="text-sm text-destructive">
          Could not load devices (HTTP {devices.data.status}).
        </p>
      ) : (
        <div className="rounded-md border">
          <Table>
            <TableHeader>
              {table.getHeaderGroups().map((hg) => (
                <TableRow key={hg.id}>
                  {hg.headers.map((h) => (
                    <TableHead key={h.id}>
                      {h.isPlaceholder
                        ? null
                        : flexRender(h.column.columnDef.header, h.getContext())}
                    </TableHead>
                  ))}
                </TableRow>
              ))}
            </TableHeader>
            <TableBody>
              {table.getRowModel().rows.length === 0 ? (
                <TableRow>
                  <TableCell
                    colSpan={columns.length}
                    className="h-24 text-center text-muted-foreground"
                  >
                    {devices.isLoading
                      ? 'Loading…'
                      : filter
                        ? 'No devices match the filter.'
                        : 'No devices enrolled yet.'}
                  </TableCell>
                </TableRow>
              ) : (
                table.getRowModel().rows.map((row) => (
                  <TableRow
                    key={row.id}
                    className="cursor-pointer"
                    onClick={() =>
                      navigate({
                        to: '/devices/$device-id',
                        params: { 'device-id': row.original.deviceId },
                      })
                    }
                  >
                    {row.getVisibleCells().map((cell) => (
                      <TableCell key={cell.id}>
                        {flexRender(cell.column.columnDef.cell, cell.getContext())}
                      </TableCell>
                    ))}
                  </TableRow>
                ))
              )}
            </TableBody>
          </Table>
        </div>
      )}
    </div>
  )
}
