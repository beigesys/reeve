import { Badge } from '@/components/ui/badge'
import { cn } from '@/lib/utils'

const ROLLOUT_STATE: Record<string, string> = {
  active: 'border-blue-500/40 text-blue-600 dark:text-blue-400',
  paused: 'border-amber-500/40 text-amber-600 dark:text-amber-400',
  aborted: 'border-red-500/40 text-red-600 dark:text-red-400',
  completed: 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400',
}

/** Rollout lifecycle badge (spec/reeve/09-rollouts.md §11.6). */
export function RolloutStateBadge({ state }: { state: string }) {
  return (
    <Badge variant="outline" className={cn('font-normal', ROLLOUT_STATE[state])}>
      {state}
    </Badge>
  )
}

const WAVE_STATE: Record<string, string> = {
  pending: 'text-muted-foreground',
  advancing: 'border-blue-500/40 text-blue-600 dark:text-blue-400',
  soaking: 'border-blue-500/40 text-blue-600 dark:text-blue-400',
  passed: 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400',
  failed: 'border-red-500/40 text-red-600 dark:text-red-400',
}

export function WaveStateBadge({ state }: { state: string }) {
  return (
    <Badge variant="outline" className={cn('font-normal', WAVE_STATE[state])}>
      {state}
    </Badge>
  )
}

const DEVICE_CLASS: Record<string, string> = {
  pending: 'text-muted-foreground',
  advanced: 'border-blue-500/40 text-blue-600 dark:text-blue-400',
  converged: 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400',
  healthy: 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400',
  undetermined: 'border-amber-500/40 text-amber-600 dark:text-amber-400',
  failed: 'border-red-500/40 text-red-600 dark:text-red-400',
}

/** §11.6 per-device rollout classification badge. */
export function DeviceClassBadge({ state }: { state: string }) {
  return (
    <Badge variant="outline" className={cn('font-normal', DEVICE_CLASS[state])}>
      {state}
    </Badge>
  )
}
