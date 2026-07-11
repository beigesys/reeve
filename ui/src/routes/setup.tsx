import { useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { Link, createFileRoute, useNavigate } from '@tanstack/react-router'
import { getMeQueryKey, useSetup } from '@/api/endpoints/auth/auth'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

export const Route = createFileRoute('/setup')({
  component: SetupPage,
})

/**
 * First-boot admin creation (docs/decisions/auth.md D1): valid only
 * while zero users exist, with the one-time setup token logged at
 * server startup.
 */
function SetupPage() {
  const [setupToken, setSetupToken] = useState('')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const queryClient = useQueryClient()
  const navigate = useNavigate()

  const setup = useSetup({
    mutation: {
      onSuccess: async (res) => {
        if (res.status === 201) {
          await queryClient.invalidateQueries({ queryKey: getMeQueryKey() })
          await navigate({ to: '/devices' })
          return
        }
        setError(
          res.status === 401
            ? 'Wrong setup token — check the token logged at server startup.'
            : res.status === 409
              ? 'Setup window closed: an admin already exists. Sign in instead.'
              : res.status === 422
                ? 'Username and password must not be empty.'
                : 'Setup is unavailable (server not in password auth mode).',
        )
      },
      onError: () => setError('Could not reach the server.'),
    },
  })

  return (
    <div className="flex min-h-screen items-center justify-center p-6">
      <Card className="w-full max-w-sm">
        <CardHeader>
          <CardTitle className="text-xl">First-boot setup</CardTitle>
          <CardDescription>
            Create the admin account with the one-time setup token from the
            server log.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <form
            className="flex flex-col gap-4"
            onSubmit={(e) => {
              e.preventDefault()
              setError(null)
              setup.mutate({
                data: { setup_token: setupToken, username, password },
              })
            }}
          >
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="setup-token">Setup token</Label>
              <Input
                id="setup-token"
                value={setupToken}
                onChange={(e) => setSetupToken(e.target.value)}
                required
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="username">Admin username</Label>
              <Input
                id="username"
                autoComplete="username"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                required
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="password">Password</Label>
              <Input
                id="password"
                type="password"
                autoComplete="new-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
              />
            </div>
            {error && <p className="text-sm text-destructive">{error}</p>}
            <Button type="submit" disabled={setup.isPending}>
              {setup.isPending ? 'Creating…' : 'Create admin'}
            </Button>
            <p className="text-center text-sm text-muted-foreground">
              Already set up?{' '}
              <Link to="/login" className="underline underline-offset-4">
                Sign in
              </Link>
            </p>
          </form>
        </CardContent>
      </Card>
    </div>
  )
}
