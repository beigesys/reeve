import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowLeft } from 'lucide-react'
import { useGroupsList } from '@/api/endpoints/groups/groups'
import type { GroupNode } from '@/api/model'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { GroupForm } from '@/components/group-form'

export const Route = createFileRoute('/_app/groups/$group-id/edit')({
  component: EditGroupPage,
})

function EditGroupPage() {
  const params = Route.useParams()
  const id = Number(params['group-id'])
  const tree = useGroupsList(undefined)
  const fleets = tree.data?.status === 200 ? tree.data.data.fleets : []

  // Resolve the group (and, for a site, its containing fleet) from the tree —
  // there is no single-group GET; the tree is the source of truth.
  let group: GroupNode | undefined
  let parentName: string | undefined
  for (const f of fleets) {
    if (f.id === id) {
      group = { id: f.id, kind: 'fleet', name: f.name, parentId: null }
      break
    }
    const s = f.sites.find((x) => x.id === id)
    if (s) {
      group = { id: s.id, kind: 'site', name: s.name, parentId: f.id }
      parentName = f.name
      break
    }
  }

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/fleet">
            <ArrowLeft className="size-4" />
            Fleet
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">
          {group ? `Manage ${group.kind} ${group.name}` : 'Manage group'}
        </h1>
      </div>

      <Card className="max-w-lg">
        <CardHeader>
          <CardTitle className="text-base">Rename or delete</CardTitle>
        </CardHeader>
        <CardContent>
          {tree.isLoading ? (
            <p className="text-sm text-muted-foreground">Loading…</p>
          ) : !group ? (
            <p className="text-sm text-destructive">
              That group no longer exists.
            </p>
          ) : (
            <GroupForm mode="rename" group={group} parentName={parentName} />
          )}
        </CardContent>
      </Card>
    </div>
  )
}
