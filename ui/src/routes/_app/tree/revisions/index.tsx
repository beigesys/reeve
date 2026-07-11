import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowLeft } from 'lucide-react'
import { useListRevisions } from '@/api/endpoints/tree/tree'
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
import { fmtRfc3339 } from '@/lib/format'
import { usePollInterval } from '@/lib/sse'

export const Route = createFileRoute('/_app/tree/revisions/')({
  component: RevisionsPage,
})

function RevisionsPage() {
  const refetchInterval = usePollInterval(30_000)
  const revs = useListRevisions({ limit: 500 }, { query: { refetchInterval } })
  const rows = revs.data?.status === 200 ? revs.data.data : []

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/tree">
            <ArrowLeft className="size-4" />
            Advanced
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">Change log</h1>
      </div>

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Change</TableHead>
              <TableHead>Source</TableHead>
              <TableHead>Message</TableHead>
              <TableHead>Author</TableHead>
              <TableHead>Created</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.length === 0 ? (
              <TableRow>
                <TableCell
                  colSpan={5}
                  className="h-16 text-center text-muted-foreground"
                >
                  {revs.isLoading ? 'Loading…' : 'No revisions yet.'}
                </TableCell>
              </TableRow>
            ) : (
              rows.map((r) => (
                <TableRow key={r.id}>
                  <TableCell>
                    <Link
                      to="/tree/revisions/$revision-id"
                      params={{ 'revision-id': String(r.id) }}
                      className="font-mono text-sm underline-offset-4 hover:underline"
                    >
                      r{r.id}
                    </Link>
                  </TableCell>
                  <TableCell>
                    <Badge
                      variant={r.stream === 'local' ? 'secondary' : 'outline'}
                      className="font-normal"
                    >
                      {r.stream}
                    </Badge>
                  </TableCell>
                  <TableCell className="max-w-96 truncate text-sm">
                    {r.message}
                  </TableCell>
                  <TableCell className="text-sm">{r.author}</TableCell>
                  <TableCell className="text-sm text-muted-foreground">
                    {fmtRfc3339(r.created_at)}
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
