import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowLeft } from 'lucide-react'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { SecretForm } from '@/components/secret-form'

interface SetSearch {
  name?: string
  scope?: string
}

export const Route = createFileRoute('/_app/secrets/set')({
  validateSearch: (search: Record<string, unknown>): SetSearch => ({
    name: typeof search.name === 'string' ? search.name : undefined,
    scope: typeof search.scope === 'string' ? search.scope : undefined,
  }),
  component: SecretSetPage,
})

/** New and rotate share this page — same PUT, same form (D14-style). */
function SecretSetPage() {
  const { name, scope } = Route.useSearch()
  const rotating = !!name

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/secrets">
            <ArrowLeft className="size-4" />
            Secrets
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">
          {rotating ? `Rotate ${name}` : 'Set secret'}
        </h1>
      </div>

      <Card className="max-w-2xl">
        <CardHeader>
          <CardTitle className="text-base">
            {rotating ? 'New value' : 'New secret'}
          </CardTitle>
          <CardDescription>
            Writing an existing (scope, name) rotates it: the version bumps
            and exactly the consuming services re-up
            (spec/reeve/10-secrets.md §12).
          </CardDescription>
        </CardHeader>
        <CardContent>
          <SecretForm
            key={`${scope ?? ''}/${name ?? ''}`}
            initialName={name ?? ''}
            initialScope={scope ?? 'fleet'}
          />
        </CardContent>
      </Card>
    </div>
  )
}
