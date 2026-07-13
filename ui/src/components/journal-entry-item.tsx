import { useState } from 'react'
import { ChevronRight } from 'lucide-react'
import type {
  DeploymentStatusManifest,
  HealthSample,
  JournalEntry,
} from '@/api/model'
import { Badge } from '@/components/ui/badge'
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from '@/components/ui/collapsible'
import { ScrollArea } from '@/components/ui/scroll-area'
import { cn } from '@/lib/utils'
import { fmtRfc3339, fmtUnix } from '@/lib/format'

type Obj = Record<string, unknown>
const asObj = (v: unknown): Obj | undefined =>
  v != null && typeof v === 'object' ? (v as Obj) : undefined

/** Largest used% across the reported filesystems, if derivable. */
function peakDiskPct(disk: unknown): { mount: string; pct: number } | null {
  const d = asObj(disk)
  if (!d) return null
  let best: { mount: string; pct: number } | null = null
  for (const [mount, sample] of Object.entries(d)) {
    const s = asObj(sample)
    const used = typeof s?.usedBytes === 'number' ? s.usedBytes : null
    const total = typeof s?.totalBytes === 'number' ? s.totalBytes : null
    if (used == null || total == null || total <= 0) continue
    const pct = Math.round((used / total) * 100)
    if (!best || pct > best.pct) best = { mount, pct }
  }
  return best
}

/** One concise human line derived from a journal entry's payload. */
function summarize(record: JournalEntry): string {
  const p = record.payload
  switch (record.kind) {
    case 'status': {
      const m = asObj(p) as DeploymentStatusManifest | undefined
      if (!m || typeof m.deploymentId !== 'string') return 'Status report'
      const state = m.status?.state ?? 'unknown'
      const err = m.status?.error?.message
      const base = `Deployment ${m.deploymentId}: ${state}`
      return err ? `${base} — ${err}` : base
    }
    case 'health': {
      const h = asObj(p) as HealthSample | undefined
      const parts: string[] = []
      if (h?.load && h.load.length > 0)
        parts.push(`load ${h.load[0].toFixed(2)}`)
      const used = h?.memory?.usedBytes
      const total = h?.memory?.totalBytes
      if (typeof used === 'number' && typeof total === 'number' && total > 0)
        parts.push(`mem ${Math.round((used / total) * 100)}%`)
      const disk = peakDiskPct(h?.disk)
      if (disk) parts.push(`disk ${disk.mount} ${disk.pct}%`)
      return parts.length > 0 ? parts.join(' · ') : 'Health sample'
    }
    case 'lifecycle': {
      const o = asObj(p)
      const event = o?.event ?? o?.type ?? o?.kind ?? o?.phase ?? o?.message
      if (typeof event === 'string' && event.length > 0) return event
      return 'Agent lifecycle event'
    }
    case 'gap': {
      const o = asObj(p)
      const n = o?.dropped ?? o?.count ?? o?.records ?? o?.missing
      if (typeof n === 'number') return `${n} records dropped`
      return 'Records dropped (gap in the journal)'
    }
    default:
      return record.kind
  }
}

/** Kind pill; gaps read as an attention state, the rest are neutral. */
function KindBadge({ kind }: { kind: string }) {
  const tone =
    kind === 'gap'
      ? 'border-amber-500/40 text-amber-600 dark:text-amber-400'
      : kind === 'lifecycle'
        ? 'border-sky-500/40 text-sky-600 dark:text-sky-400'
        : 'text-muted-foreground'
  return (
    <Badge variant="outline" className={cn('font-normal', tone)}>
      {kind}
    </Badge>
  )
}

/**
 * One journal entry as a timeline row: kind + timestamp + a concise
 * human summary, expandable to reveal the pretty-printed raw payload
 * (contained in an internal scroll — it never widens the page).
 */
export function JournalEntryItem({ record }: { record: JournalEntry }) {
  const [open, setOpen] = useState(false)
  const summary = summarize(record)
  const hasPayload = record.payload != null
  return (
    <Collapsible
      open={open}
      onOpenChange={setOpen}
      className="border-b last:border-b-0"
    >
      <CollapsibleTrigger
        className={cn(
          'flex w-full min-w-0 items-start gap-2 px-4 py-3 text-left hover:bg-muted/40',
          !hasPayload && 'cursor-default hover:bg-transparent',
        )}
        disabled={!hasPayload}
      >
        <ChevronRight
          className={cn(
            'mt-0.5 size-4 shrink-0 text-muted-foreground transition-transform',
            open && 'rotate-90',
            !hasPayload && 'invisible',
          )}
        />
        <div className="flex min-w-0 flex-1 flex-col gap-1">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
            <KindBadge kind={record.kind} />
            <span>{fmtRfc3339(record.observedAt)}</span>
            <span>·</span>
            <span>received {fmtUnix(record.receivedAt)}</span>
            <span>·</span>
            <span>seq {record.seq}</span>
          </div>
          <p className="text-sm break-words">{summary}</p>
        </div>
      </CollapsibleTrigger>
      {hasPayload && (
        <CollapsibleContent>
          <div className="px-4 pb-3 pl-10">
            <ScrollArea className="max-h-80 min-w-0 rounded border bg-muted">
              <pre className="p-3 font-mono text-xs break-words whitespace-pre-wrap">
                {JSON.stringify(record.payload, null, 2)}
              </pre>
            </ScrollArea>
          </div>
        </CollapsibleContent>
      )}
    </Collapsible>
  )
}
