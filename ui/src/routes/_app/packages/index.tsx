import { Link, createFileRoute } from '@tanstack/react-router'
import { Upload } from 'lucide-react'
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
import { groupPackages, useHeadFiles } from '@/lib/tree'

export const Route = createFileRoute('/_app/packages/')({
  component: PackagesPage,
})

/**
 * App catalog: vendored Margo application packages at head
 * (`packages/<name>/<version>/**`, docs/decisions/tree-render.md D11).
 */
function PackagesPage() {
  const { files, streamOf, isLoading } = useHeadFiles()
  const packages = files ? [...groupPackages(files).values()] : []

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-xl font-semibold tracking-tight">Packages</h1>
        <Button variant="outline" size="sm" asChild>
          <Link to="/packages/new">
            <Upload className="size-4" />
            Upload package
          </Link>
        </Button>
      </div>

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Package</TableHead>
              <TableHead>Version</TableHead>
              <TableHead>Files</TableHead>
              <TableHead>Stream</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {packages.length === 0 ? (
              <TableRow>
                <TableCell
                  colSpan={4}
                  className="h-16 text-center text-muted-foreground"
                >
                  {isLoading ? 'Loading…' : 'No packages vendored yet.'}
                </TableCell>
              </TableRow>
            ) : (
              packages.map((p) => {
                const streams = new Set(
                  p.files.map((f) =>
                    streamOf(`packages/${p.name}/${p.version}/${f}`),
                  ),
                )
                return (
                  <TableRow key={`${p.name}/${p.version}`}>
                    <TableCell>
                      <Link
                        to="/packages/$name/$version"
                        params={{ name: p.name, version: p.version }}
                        className="font-medium underline-offset-4 hover:underline"
                      >
                        {p.name}
                      </Link>
                    </TableCell>
                    <TableCell className="font-mono text-sm">{p.version}</TableCell>
                    <TableCell className="text-sm text-muted-foreground">
                      {p.files.length}
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
