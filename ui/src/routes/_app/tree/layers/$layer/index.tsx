import { useState } from 'react'
import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowLeft, Pencil } from 'lucide-react'
import { useBlame } from '@/api/endpoints/tree/tree'
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
import { cn } from '@/lib/utils'
import { fmtDigest, fmtRfc3339 } from '@/lib/format'
import { useFileContent, useHeadFiles } from '@/lib/tree'

export const Route = createFileRoute('/_app/tree/layers/$layer/')({
  component: LayerPage,
})

function FileContentView({
  revisionId,
  path,
}: {
  revisionId: number
  path: string
}) {
  const content = useFileContent(revisionId, path)
  if (content.isLoading)
    return <p className="text-sm text-muted-foreground">Loading…</p>
  if (content.isError || !content.data)
    return <p className="text-sm text-destructive">Could not load file.</p>
  if (content.data.text == null)
    return (
      <p className="text-sm text-muted-foreground">
        Binary content ({content.data.size} bytes).
      </p>
    )
  return (
    <pre className="max-h-[60vh] overflow-auto rounded bg-muted p-3 font-mono text-xs">
      {content.data.text}
    </pre>
  )
}

/** Blame: every revision at which this path changed (digest null = removal). */
function BlameView({ path }: { path: string }) {
  const blame = useBlame(path, { query: { staleTime: 30_000 } })
  if (blame.isLoading)
    return <p className="text-sm text-muted-foreground">Loading…</p>
  if (blame.data?.status !== 200)
    return <p className="text-sm text-destructive">Blame unavailable.</p>
  return (
    <div className="flex flex-col divide-y">
      {blame.data.data.map((entry) => (
        <div key={`${entry.revision.stream}-${entry.revision.id}`} className="flex flex-col gap-0.5 py-2">
          <div className="flex items-center gap-2 text-sm">
            <Link
              to="/tree/revisions/$revision-id"
              params={{ 'revision-id': String(entry.revision.id) }}
              className="font-mono underline-offset-4 hover:underline"
            >
              r{entry.revision.id}
            </Link>
            <Badge
              variant={entry.revision.stream === 'local' ? 'secondary' : 'outline'}
              className="font-normal"
            >
              {entry.revision.stream}
            </Badge>
            <span>{entry.revision.message}</span>
            {entry.digest == null && (
              <Badge variant="outline" className="border-red-500/40 font-normal text-red-600 dark:text-red-400">
                removed
              </Badge>
            )}
          </div>
          <span className="text-xs text-muted-foreground">
            by {entry.revision.author}, {fmtRfc3339(entry.revision.created_at)} —{' '}
            {fmtDigest(entry.digest)}
          </span>
        </div>
      ))}
    </div>
  )
}

function LayerPage() {
  const { layer } = Route.useParams()
  const { files, streamOf, local, upstream, isLoading } = useHeadFiles()
  const [selected, setSelected] = useState<string | null>(null)

  const prefix = `layers/${layer}/`
  const layerFiles = files
    ? Object.keys(files)
        .filter((p) => p.startsWith(prefix))
        .sort()
    : []
  const selectedPath = selected ?? layerFiles[0] ?? null
  const selectedStream = selectedPath ? streamOf(selectedPath) : null
  const contentRevision =
    selectedStream === 'local' ? local?.id : upstream?.id

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/tree">
            <ArrowLeft className="size-4" />
            Advanced
          </Link>
        </Button>
        <h1 className="font-mono text-xl font-semibold tracking-tight">{layer}</h1>
        <Button variant="outline" size="sm" asChild>
          <Link to="/tree/layers/$layer/edit" params={{ layer }}>
            <Pencil className="size-4" />
            Edit layer
          </Link>
        </Button>
      </div>

      {isLoading ? (
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : layerFiles.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          This layer has no files at head.{' '}
          <Link
            to="/tree/layers/$layer/edit"
            params={{ layer }}
            className="underline underline-offset-4"
          >
            Add some
          </Link>
          .
        </p>
      ) : (
        <div className="flex gap-4">
          <Card className="w-72 shrink-0">
            <CardHeader>
              <CardTitle className="text-base">Files</CardTitle>
            </CardHeader>
            <CardContent className="flex flex-col gap-0.5 p-2">
              {layerFiles.map((path) => (
                <button
                  key={path}
                  type="button"
                  onClick={() => setSelected(path)}
                  className={cn(
                    'rounded px-2 py-1 text-left font-mono text-xs hover:bg-accent',
                    path === selectedPath && 'bg-accent font-medium',
                  )}
                >
                  {path.slice(prefix.length)}
                </button>
              ))}
            </CardContent>
          </Card>

          {selectedPath && (
            <Card className="min-w-0 flex-1">
              <CardHeader>
                <CardTitle className="flex items-center gap-2 font-mono text-base">
                  {selectedPath.slice(prefix.length)}
                  {selectedStream && (
                    <Badge
                      variant={selectedStream === 'local' ? 'secondary' : 'outline'}
                      className="font-normal"
                    >
                      {selectedStream}
                    </Badge>
                  )}
                </CardTitle>
                <CardDescription className="font-mono text-xs">
                  {selectedPath}
                </CardDescription>
              </CardHeader>
              <CardContent>
                <Tabs defaultValue="content">
                  <TabsList>
                    <TabsTrigger value="content">Content</TabsTrigger>
                    <TabsTrigger value="blame">Blame</TabsTrigger>
                  </TabsList>
                  <TabsContent value="content">
                    {contentRevision != null && (
                      <FileContentView
                        revisionId={contentRevision}
                        path={selectedPath}
                      />
                    )}
                  </TabsContent>
                  <TabsContent value="blame">
                    <BlameView path={selectedPath} />
                  </TabsContent>
                </Tabs>
              </CardContent>
            </Card>
          )}
        </div>
      )}
    </div>
  )
}
