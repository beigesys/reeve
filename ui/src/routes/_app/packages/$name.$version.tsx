import { useState } from 'react'
import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowLeft } from 'lucide-react'
import { parse as parseYaml } from 'yaml'
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
import { cn } from '@/lib/utils'
import { useFileContent, useHeadFiles } from '@/lib/tree'

export const Route = createFileRoute('/_app/packages/$name/$version')({
  component: PackageDetailPage,
})

// Presentation-only view of a parsed margo.yaml. This is FILE content
// (spec/margo ApplicationDescription), not an API wire type — the
// authoritative parse lives in crates/margo-package; the UI renders
// best-effort and always shows the raw YAML alongside.
interface MargoDoc {
  apiVersion?: string
  kind?: string
  id?: string
  metadata?: {
    id?: string
    name?: string
    version?: string
    description?: string
  }
  deploymentProfiles?: {
    type?: string
    id?: string
    description?: string
    components?: { name?: string; properties?: unknown }[]
  }[]
  parameters?: Record<string, { value?: unknown; targets?: unknown[] }>
  configuration?: unknown
}

function tryParseMargo(text: string): MargoDoc | null {
  try {
    const doc: unknown = parseYaml(text)
    return typeof doc === 'object' && doc != null ? (doc as MargoDoc) : null
  } catch {
    return null
  }
}

function MargoSummary({ doc }: { doc: MargoDoc }) {
  const id = doc.id ?? doc.metadata?.id
  const params = Object.entries(doc.parameters ?? {})
  return (
    <div className="flex flex-col gap-4">
      <div className="grid grid-cols-2 gap-4 md:grid-cols-4">
        {[
          ['Name', doc.metadata?.name],
          ['Id', id],
          ['Version', doc.metadata?.version],
          ['API version', doc.apiVersion],
          ['Kind', doc.kind],
          ['Description', doc.metadata?.description],
        ].map(([label, value]) => (
          <div key={label} className="flex flex-col gap-0.5">
            <span className="text-xs text-muted-foreground">{label}</span>
            <span className="text-sm">{value ?? '—'}</span>
          </div>
        ))}
      </div>

      <div>
        <h3 className="mb-2 text-sm font-medium">Deployment profiles</h3>
        {(doc.deploymentProfiles ?? []).length === 0 ? (
          <p className="text-sm text-muted-foreground">None declared.</p>
        ) : (
          <div className="flex flex-col gap-3">
            {(doc.deploymentProfiles ?? []).map((p, i) => (
              <div key={p.id ?? i} className="rounded-md border p-3">
                <div className="mb-2 flex items-center gap-2">
                  <Badge variant="secondary" className="font-mono font-normal">
                    {p.type ?? 'unknown'}
                  </Badge>
                  {p.id && <span className="font-mono text-xs">{p.id}</span>}
                  {p.description && (
                    <span className="text-xs text-muted-foreground">
                      {p.description}
                    </span>
                  )}
                </div>
                {(p.components ?? []).map((c, j) => (
                  <div key={c.name ?? j} className="mb-2 last:mb-0">
                    <span className="font-mono text-xs font-medium">
                      {c.name ?? `component ${j}`}
                    </span>
                    {c.properties != null && (
                      <pre className="mt-1 overflow-x-auto rounded bg-muted p-2 font-mono text-xs">
                        {JSON.stringify(c.properties, null, 2)}
                      </pre>
                    )}
                  </div>
                ))}
              </div>
            ))}
          </div>
        )}
      </div>

      <div>
        <h3 className="mb-2 text-sm font-medium">Parameters</h3>
        {params.length === 0 ? (
          <p className="text-sm text-muted-foreground">None declared.</p>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Value</TableHead>
                <TableHead>Targets</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {params.map(([name, p]) => (
                <TableRow key={name}>
                  <TableCell className="font-mono text-xs">{name}</TableCell>
                  <TableCell className="font-mono text-xs">
                    {p?.value != null ? JSON.stringify(p.value) : '—'}
                  </TableCell>
                  <TableCell className="text-xs text-muted-foreground">
                    {p?.targets?.length ?? 0}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </div>
    </div>
  )
}

function PackageDetailPage() {
  const { name, version } = Route.useParams()
  const { files, streamOf, local, upstream, isLoading } = useHeadFiles()
  const [selected, setSelected] = useState<string | null>(null)

  const prefix = `packages/${name}/${version}/`
  const pkgFiles = files
    ? Object.keys(files)
        .filter((p) => p.startsWith(prefix))
        .sort()
    : []

  const margoPath = `${prefix}margo.yaml`
  const hasMargo = files != null && margoPath in files
  const margoRev = hasMargo
    ? streamOf(margoPath) === 'local'
      ? local?.id
      : upstream?.id
    : undefined
  const margo = useFileContent(margoRev, hasMargo ? margoPath : undefined)
  const doc = margo.data?.text != null ? tryParseMargo(margo.data.text) : null

  const selectedPath = selected ?? margoPath
  const selectedRev =
    files && selectedPath in files
      ? streamOf(selectedPath) === 'local'
        ? local?.id
        : upstream?.id
      : undefined
  const content = useFileContent(selectedRev, selectedPath)

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/packages">
            <ArrowLeft className="size-4" />
            Packages
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">
          {name}
          <span className="ml-2 font-mono text-base text-muted-foreground">
            {version}
          </span>
        </h1>
      </div>

      {isLoading ? (
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : pkgFiles.length === 0 ? (
        <p className="text-sm text-destructive">
          No such package at head.
        </p>
      ) : (
        <>
          <Card>
            <CardHeader>
              <CardTitle className="text-base">margo.yaml</CardTitle>
              <CardDescription>
                The application's package definition, parsed and validated
                server-side when the package is uploaded.
              </CardDescription>
            </CardHeader>
            <CardContent>
              {!hasMargo ? (
                <p className="text-sm text-destructive">
                  Package has no margo.yaml.
                </p>
              ) : margo.isLoading ? (
                <p className="text-sm text-muted-foreground">Loading…</p>
              ) : doc ? (
                <MargoSummary doc={doc} />
              ) : (
                <p className="text-sm text-destructive">
                  margo.yaml did not parse as YAML — raw content below.
                </p>
              )}
            </CardContent>
          </Card>

          <div className="flex gap-4">
            <Card className="w-72 shrink-0">
              <CardHeader>
                <CardTitle className="text-base">Files</CardTitle>
              </CardHeader>
              <CardContent className="flex flex-col gap-0.5 p-2">
                {pkgFiles.map((path) => (
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

            <Card className="min-w-0 flex-1">
              <CardHeader>
                <CardTitle className="font-mono text-base">
                  {selectedPath.slice(prefix.length)}
                </CardTitle>
              </CardHeader>
              <CardContent>
                {content.isLoading ? (
                  <p className="text-sm text-muted-foreground">Loading…</p>
                ) : content.data?.text != null ? (
                  <pre className="max-h-[60vh] overflow-auto rounded bg-muted p-3 font-mono text-xs">
                    {content.data.text}
                  </pre>
                ) : content.data ? (
                  <p className="text-sm text-muted-foreground">
                    Binary content ({content.data.size} bytes).
                  </p>
                ) : (
                  <p className="text-sm text-destructive">Could not load file.</p>
                )}
              </CardContent>
            </Card>
          </div>
        </>
      )}
    </div>
  )
}
