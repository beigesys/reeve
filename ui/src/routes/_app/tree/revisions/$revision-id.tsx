import { useState } from 'react'
import { Link, createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { ArrowLeft } from 'lucide-react'
import { useDiff, useGetRevision, usePutLayer } from '@/api/endpoints/tree/tree'
import type { DiffEntry } from '@/api/model'
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
import { fmtDigest, fmtRfc3339 } from '@/lib/format'
import { groupLayers, loadFileContent } from '@/lib/tree'

export const Route = createFileRoute('/_app/tree/revisions/$revision-id')({
  component: RevisionDetailPage,
})

function changeBadge(change: string) {
  const cls =
    change === 'added'
      ? 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400'
      : change === 'removed'
        ? 'border-red-500/40 text-red-600 dark:text-red-400'
        : 'border-amber-500/40 text-amber-600 dark:text-amber-400'
  return (
    <Badge variant="outline" className={`font-normal ${cls}`}>
      {change}
    </Badge>
  )
}

function DiffTable({ entries }: { entries: DiffEntry[] }) {
  if (entries.length === 0)
    return (
      <p className="text-sm text-muted-foreground">
        No changes against the parent revision.
      </p>
    )
  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>Change</TableHead>
          <TableHead>Path</TableHead>
          <TableHead>Old</TableHead>
          <TableHead>New</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {entries.map((e) => (
          <TableRow key={e.path}>
            <TableCell>{changeBadge(e.change)}</TableCell>
            <TableCell className="font-mono text-xs">{e.path}</TableCell>
            <TableCell className="font-mono text-xs text-muted-foreground">
              {fmtDigest(e.old)}
            </TableCell>
            <TableCell className="font-mono text-xs text-muted-foreground">
              {fmtDigest(e.new)}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  )
}

/**
 * Revert = a NEW revision whose layer content equals this revision's
 * (history is append-only and never rewritten): bulk-read the layer's
 * files at this revision, PUT them as the complete replacement set.
 */
function RevertLayerButton({
  revisionId,
  layer,
  layerFiles,
}: {
  revisionId: number
  layer: string
  layerFiles: string[]
}) {
  const qc = useQueryClient()
  const put = usePutLayer()
  const [state, setState] = useState<'idle' | 'working' | 'done' | 'error'>('idle')
  const [detail, setDetail] = useState('')

  const revert = async () => {
    setState('working')
    try {
      const files: Record<string, string> = {}
      for (const rel of layerFiles) {
        const content = await loadFileContent(revisionId, `layers/${layer}/${rel}`)
        files[rel] = content.base64
      }
      const res = await put.mutateAsync({
        layer,
        data: { files, message: `revert ${layer} to r${revisionId}` },
      })
      if (res.status === 200) {
        setState('done')
        setDetail(
          res.data.changed
            ? `committed r${res.data.revision}`
            : 'head already matches (no new revision)',
        )
        void qc.invalidateQueries()
      } else {
        setState('error')
        setDetail(`HTTP ${res.status}`)
      }
    } catch (e) {
      setState('error')
      setDetail(e instanceof Error ? e.message : 'failed')
    }
  }

  return (
    <span className="flex items-center gap-2">
      <ConfirmButton
        label={`Revert ${layer} to r${revisionId}`}
        confirmLabel="Commit revert?"
        disabled={state === 'working'}
        onConfirm={() => void revert()}
      />
      {state === 'working' && (
        <span className="text-xs text-muted-foreground">Reverting…</span>
      )}
      {state === 'done' && (
        <span className="text-xs text-emerald-600 dark:text-emerald-400">
          {detail}
        </span>
      )}
      {state === 'error' && (
        <span className="text-xs text-destructive">{detail}</span>
      )}
    </span>
  )
}

function RevisionDetailPage() {
  const params = Route.useParams()
  const id = Number(params['revision-id'])
  const detail = useGetRevision(id)
  const data = detail.data?.status === 200 ? detail.data.data : undefined
  const parent = data?.revision.parent ?? null
  const diff = useDiff(parent ?? -1, id, {
    query: { enabled: parent != null, staleTime: Infinity },
  })

  const layers = data ? groupLayers(data.files) : new Map<string, string[]>()

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/tree/revisions">
            <ArrowLeft className="size-4" />
            Revisions
          </Link>
        </Button>
        <h1 className="font-mono text-xl font-semibold tracking-tight">r{id}</h1>
        {data && (
          <Badge
            variant={data.revision.stream === 'local' ? 'secondary' : 'outline'}
            className="font-normal"
          >
            {data.revision.stream}
          </Badge>
        )}
      </div>

      {detail.data && detail.data.status !== 200 ? (
        <p className="text-sm text-destructive">Unknown revision.</p>
      ) : !data ? (
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : (
        <>
          <Card>
            <CardHeader>
              <CardTitle className="text-base">{data.revision.message}</CardTitle>
              <CardDescription>
                by {data.revision.author}, {fmtRfc3339(data.revision.created_at)}
                {parent != null ? (
                  <>
                    {' — parent '}
                    <Link
                      to="/tree/revisions/$revision-id"
                      params={{ 'revision-id': String(parent) }}
                      className="font-mono underline-offset-4 hover:underline"
                    >
                      r{parent}
                    </Link>
                  </>
                ) : (
                  ' — initial revision (no parent)'
                )}
              </CardDescription>
            </CardHeader>
            {layers.size > 0 && data.revision.stream === 'local' && (
              <CardContent className="flex flex-wrap gap-2">
                {[...layers.entries()].map(([layer, layerFiles]) => (
                  <RevertLayerButton
                    key={layer}
                    revisionId={id}
                    layer={layer}
                    layerFiles={layerFiles}
                  />
                ))}
              </CardContent>
            )}
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-base">Diff vs parent</CardTitle>
            </CardHeader>
            <CardContent>
              {parent == null ? (
                <p className="text-sm text-muted-foreground">
                  Initial revision — every file below is new.
                </p>
              ) : diff.data?.status === 200 ? (
                <DiffTable entries={diff.data.data} />
              ) : (
                <p className="text-sm text-muted-foreground">
                  {diff.isLoading ? 'Loading…' : 'Diff unavailable.'}
                </p>
              )}
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-base">
                Files at this revision ({Object.keys(data.files).length})
              </CardTitle>
            </CardHeader>
            <CardContent>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Path</TableHead>
                    <TableHead>Blob digest</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {Object.entries(data.files)
                    .sort(([a], [b]) => a.localeCompare(b))
                    .map(([path, digest]) => (
                      <TableRow key={path}>
                        <TableCell className="font-mono text-xs">{path}</TableCell>
                        <TableCell className="font-mono text-xs text-muted-foreground">
                          {fmtDigest(digest)}
                        </TableCell>
                      </TableRow>
                    ))}
                </TableBody>
              </Table>
            </CardContent>
          </Card>
        </>
      )}
    </div>
  )
}
