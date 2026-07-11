import { Link, createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Plus, RotateCw } from 'lucide-react'
import {
  getListSecretsQueryKey,
  useDeleteRoute,
  useListSecrets,
} from '@/api/endpoints/secrets/secrets'
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

export const Route = createFileRoute('/_app/secrets/')({
  component: SecretsPage,
})

/**
 * Secrets metadata (spec/reeve/10-secrets.md §12.2 write-only): name,
 * scope, version, timestamps. Values are NEVER readable — rotate goes
 * through the same write-only form.
 */
function SecretsPage() {
  const qc = useQueryClient()
  const refetchInterval = usePollInterval(30_000)
  const secrets = useListSecrets({ query: { refetchInterval } })
  const del = useDeleteRoute()
  const rows = secrets.data?.status === 200 ? secrets.data.data.secrets : []

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-xl font-semibold tracking-tight">Secrets</h1>
        <Button variant="outline" size="sm" asChild>
          <Link to="/secrets/set">
            <Plus className="size-4" />
            Set secret
          </Link>
        </Button>
      </div>

      <p className="text-sm text-muted-foreground">
        Write-only: values are sealed at rest and never displayed. Rotating
        bumps the version; only services consuming the secret re-up.
      </p>

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Scope</TableHead>
              <TableHead>Version</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Rotated</TableHead>
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
                  {secrets.isLoading ? 'Loading…' : 'No secrets stored.'}
                </TableCell>
              </TableRow>
            ) : (
              rows.map((s) => (
                <TableRow key={`${s.scope}/${s.name}`}>
                  <TableCell className="font-mono text-sm">{s.name}</TableCell>
                  <TableCell className="font-mono text-sm">{s.scope}</TableCell>
                  <TableCell className="text-sm">{s.version}</TableCell>
                  <TableCell className="text-sm text-muted-foreground">
                    {fmtUnix(s.created_at)}
                  </TableCell>
                  <TableCell className="text-sm text-muted-foreground">
                    {fmtUnix(s.rotated_at)}
                  </TableCell>
                  <TableCell>
                    <span className="flex justify-end gap-2">
                      <Button variant="outline" size="sm" asChild>
                        <Link
                          to="/secrets/set"
                          search={{ name: s.name, scope: s.scope }}
                        >
                          <RotateCw className="size-4" />
                          Rotate
                        </Link>
                      </Button>
                      <ConfirmButton
                        label="Delete"
                        confirmLabel="Really delete?"
                        disabled={del.isPending}
                        onConfirm={() =>
                          del.mutate(
                            { scope: s.scope, name: s.name },
                            {
                              onSuccess: () =>
                                void qc.invalidateQueries({
                                  queryKey: getListSecretsQueryKey(),
                                }),
                            },
                          )
                        }
                      />
                    </span>
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
