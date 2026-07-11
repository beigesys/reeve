import { Badge } from '@/components/ui/badge'
import { cn } from '@/lib/utils'
import { fmtAgo } from '@/lib/format'
import type { PresenceInfo } from '@/api/model'

/**
 * online/offline pill with "since" (spec/reeve/02-channel.md §4.3
 * vocabulary as surfaced by GET /api/devices).
 */
export function PresenceBadge({ presence }: { presence: PresenceInfo }) {
  const online = presence.state === 'online'
  return (
    <Badge
      variant="outline"
      className={cn(
        'gap-1.5 font-normal',
        online
          ? 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400'
          : 'text-muted-foreground',
      )}
    >
      <span
        className={cn(
          'size-1.5 rounded-full',
          online ? 'bg-emerald-500' : 'bg-muted-foreground/50',
        )}
      />
      {online ? 'online' : 'offline'}
      <span className="text-muted-foreground">
        {presence.since != null ? fmtAgo(presence.since) : 'never seen'}
      </span>
    </Badge>
  )
}
