import { Badge } from '@/components/ui/badge'
import { cn } from '@/lib/utils'

/**
 * Margo deployment state pill (`pending` … `failed`;
 * margo `DeploymentStatusManifest.status.state`). The server surfaces
 * the state as a plain string, so unknown values render neutrally
 * (tolerant reader).
 */
export function DeploymentStateBadge({ state }: { state: string }) {
  const tone =
    state === 'installed'
      ? 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400'
      : state === 'failed'
        ? 'border-red-500/40 text-red-600 dark:text-red-400'
        : state === 'pending' || state === 'installing' || state === 'removing'
          ? 'border-amber-500/40 text-amber-600 dark:text-amber-400'
          : 'text-muted-foreground'
  return (
    <Badge variant="outline" className={cn('font-normal', tone)}>
      {state}
    </Badge>
  )
}
