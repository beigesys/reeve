import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowLeft } from 'lucide-react'
import { useGroupsList } from '@/api/endpoints/groups/groups'
import type { GroupKind } from '@/api/model'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { GroupForm } from '@/components/group-form'

type NewGroupSearch = { kind: GroupKind; parent?: number }

export const Route = createFileRoute('/_app/groups/new')({
  validateSearch: (search: Record<string, unknown>): NewGroupSearch => ({
    kind: search.kind === 'site' ? 'site' : 'fleet',
    parent:
      search.parent != null && !Number.isNaN(Number(search.parent))
        ? Number(search.parent)
        : undefined,
  }),
  component: NewGroupPage,
})

function NewGroupPage() {
  const { kind, parent } = Route.useSearch()
  const tree = useGroupsList(undefined)
  const fleets = tree.data?.status === 200 ? tree.data.data.fleets : []
  const parentFleet =
    parent != null ? fleets.find((f) => f.id === parent) : undefined

  // A site needs a valid containing fleet; guard the case where the id is
  // missing or stale (the fleet was deleted) rather than posting a bad body.
  const siteWithoutFleet = kind === 'site' && parent != null && !parentFleet

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
          {kind === 'fleet' ? 'New fleet' : 'New site'}
        </h1>
      </div>

      <Card className="max-w-lg">
        <CardHeader>
          <CardTitle className="text-base">
            {kind === 'fleet'
              ? 'Add a fleet'
              : parentFleet
                ? `Add a site to ${parentFleet.name}`
                : 'Add a site'}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {kind === 'site' && parent == null ? (
            <p className="text-sm text-muted-foreground">
              A site must be created under a fleet. Go back to the fleet and use
              its “Add site” action.
            </p>
          ) : siteWithoutFleet ? (
            <p className="text-sm text-destructive">
              That fleet no longer exists. Go back and pick a current fleet.
            </p>
          ) : kind === 'site' ? (
            <GroupForm
              mode="create"
              kind="site"
              parentId={parent}
              parentName={parentFleet?.name}
            />
          ) : (
            <GroupForm mode="create" kind="fleet" />
          )}
        </CardContent>
      </Card>
    </div>
  )
}
