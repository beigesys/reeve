import { useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { Link, createFileRoute, useNavigate } from '@tanstack/react-router'
import { getMeQueryKey, useLogin } from '@/api/endpoints/auth/auth'
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

export const Route = createFileRoute('/login')({
  component: LoginPage,
})

function LoginPage() {
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const queryClient = useQueryClient()
  const navigate = useNavigate()

  const login = useLogin({
    mutation: {
      onSuccess: async (res) => {
        if (res.status === 200) {
          // Drop the cached (anonymous) `me` so the /_app guard's
          // ensureQueryData refetches the now-authenticated session
          // instead of reading the stale copy and bouncing back here.
          queryClient.removeQueries({ queryKey: getMeQueryKey() })
          await navigate({ to: '/devices' })
        } else if (res.status === 401) {
          setError('Bad credentials.')
        } else {
          // 404: not in password auth mode — nothing to log in to.
          queryClient.removeQueries({ queryKey: getMeQueryKey() })
          await navigate({ to: '/devices' })
        }
      },
      onError: () => setError('Could not reach the server.'),
    },
  })

  return (
    <div className="flex min-h-screen items-center justify-center p-6">
      <Card className="w-full max-w-sm">
        <CardHeader>
          <CardTitle className="text-xl">reeve</CardTitle>
          <CardDescription>Sign in to manage the fleet.</CardDescription>
        </CardHeader>
        <CardContent>
          <form
            className="flex flex-col gap-4"
            onSubmit={(e) => {
              e.preventDefault()
              setError(null)
              login.mutate({ data: { username, password } })
            }}
          >
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="username">Username</Label>
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
                autoComplete="current-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
              />
            </div>
            {error && <p className="text-sm text-destructive">{error}</p>}
            <Button type="submit" disabled={login.isPending}>
              {login.isPending ? 'Signing in…' : 'Sign in'}
            </Button>
            <p className="text-center text-sm text-muted-foreground">
              First boot?{' '}
              <Link to="/setup" className="underline underline-offset-4">
                Create the admin account
              </Link>
            </p>
          </form>
        </CardContent>
      </Card>
    </div>
  )
}
