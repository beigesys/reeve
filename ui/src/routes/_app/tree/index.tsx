import { useState } from 'react'
import { Link, createFileRoute, useNavigate } from '@tanstack/react-router'
import { History, Plus } from 'lucide-react'
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
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { fmtRfc3339 } from '@/lib/format'
import { groupLayers, useHeadFiles } from '@/lib/tree'

export const Route = createFileRoute('/_app/tree/')({
  component: TreePage,
})

/** Layer directory name; the numeric prefix sets precedence order. */
const LAYER_NAME = /^\d{2}-[a-z0-9][a-z0-9.-]*$/

function TreePage() {
  const { files, streamOf, local, upstream, isLoading } = useHeadFiles()
  const navigate = useNavigate()
  const [newLayer, setNewLayer] = useState('')

  const layers = files ? groupLayers(files) : new Map<string, string[]>()
  const newLayerValid = LAYER_NAME.test(newLayer)

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center justify-between gap-4">
        <div className="flex flex-col gap-0.5">
          <h1 className="text-xl font-semibold tracking-tight">
            Advanced configuration
          </h1>
          <p className="text-sm text-muted-foreground">
            Raw config editor. Most changes are simpler through Deploy and
            Rollouts — use this only when you need direct file access.
          </p>
        </div>
        <Button variant="outline" size="sm" asChild>
          <Link to="/tree/revisions">
            <History className="size-4" />
            Change log
          </Link>
        </Button>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Current</CardTitle>
          <CardDescription>
            The configuration the next update builds from: config synced from
            upstream, with this server's own edits on top.
          </CardDescription>
        </CardHeader>
        <CardContent className="grid grid-cols-1 gap-4 md:grid-cols-2">
          {[
            { label: 'local', rev: local },
            { label: 'upstream', rev: upstream },
          ].map(({ label, rev }) => (
            <div key={label} className="flex flex-col gap-0.5">
              <span className="text-xs text-muted-foreground">
                {label} head
              </span>
              {rev ? (
                <span className="text-sm">
                  <Link
                    to="/tree/revisions/$revision-id"
                    params={{ 'revision-id': String(rev.id) }}
                    className="font-mono underline-offset-4 hover:underline"
                  >
                    r{rev.id}
                  </Link>{' '}
                  — {rev.message}{' '}
                  <span className="text-muted-foreground">
                    by {rev.author}, {fmtRfc3339(rev.created_at)}
                  </span>
                </span>
              ) : (
                <span className="text-sm text-muted-foreground">
                  {isLoading ? 'Loading…' : 'no revisions'}
                </span>
              )}
            </div>
          ))}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Config groups</CardTitle>
          <CardDescription>
            Configuration applied from fleet-wide down to a single device —
            the more specific group wins. Groups range from all devices and{' '}
            <code>fleet</code> through <code>site</code>,{' '}
            <code>device-type</code>, and an individual <code>device</code>.
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Group</TableHead>
                <TableHead>Files</TableHead>
                <TableHead>Source</TableHead>
                <TableHead />
              </TableRow>
            </TableHeader>
            <TableBody>
              {layers.size === 0 ? (
                <TableRow>
                  <TableCell
                    colSpan={4}
                    className="h-16 text-center text-muted-foreground"
                  >
                    {isLoading ? 'Loading…' : 'No layers yet.'}
                  </TableCell>
                </TableRow>
              ) : (
                [...layers.entries()].map(([layer, layerFiles]) => {
                  const streams = new Set(
                    layerFiles.map((f) => streamOf(`layers/${layer}/${f}`)),
                  )
                  return (
                    <TableRow key={layer}>
                      <TableCell>
                        <Link
                          to="/tree/layers/$layer"
                          params={{ layer }}
                          className="font-mono text-sm underline-offset-4 hover:underline"
                        >
                          {layer}
                        </Link>
                      </TableCell>
                      <TableCell className="text-sm text-muted-foreground">
                        {layerFiles.length}
                      </TableCell>
                      <TableCell>
                        <span className="flex gap-1">
                          {[...streams].map((s) => (
                            <Badge
                              key={s}
                              variant={s === 'local' ? 'secondary' : 'outline'}
                              className="font-normal"
                            >
                              {s}
                            </Badge>
                          ))}
                        </span>
                      </TableCell>
                      <TableCell className="text-right">
                        <Button variant="outline" size="sm" asChild>
                          <Link
                            to="/tree/layers/$layer/edit"
                            params={{ layer }}
                          >
                            Edit
                          </Link>
                        </Button>
                      </TableCell>
                    </TableRow>
                  )
                })
              )}
            </TableBody>
          </Table>

          <form
            className="flex items-center gap-2"
            onSubmit={(e) => {
              e.preventDefault()
              if (!newLayerValid) return
              void navigate({
                to: '/tree/layers/$layer/edit',
                params: { layer: newLayer },
              })
            }}
          >
            <Input
              placeholder="New group, e.g. 20-site.plant-a"
              value={newLayer}
              onChange={(e) => setNewLayer(e.target.value)}
              className="max-w-72 font-mono"
            />
            <Button type="submit" variant="outline" size="sm" disabled={!newLayerValid}>
              <Plus className="size-4" />
              New group
            </Button>
            {newLayer && !newLayerValid && (
              <span className="text-xs text-muted-foreground">
                Use a name like 20-site.plant-a.
              </span>
            )}
          </form>
        </CardContent>
      </Card>
    </div>
  )
}
