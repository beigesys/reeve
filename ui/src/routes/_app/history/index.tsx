import { useState } from 'react'
import { createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { ChevronDown, ChevronRight, Undo2 } from 'lucide-react'
import { useMe } from '@/api/endpoints/auth/auth'
import { getListQueryKey } from '@/api/endpoints/devices/devices'
import {
  getHistoryListQueryKey,
  useHistoryDetail,
  useHistoryList,
  useUndo,
} from '@/api/endpoints/history/history'
import type { HistoryChange } from '@/api/model'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent } from '@/components/ui/card'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'
import { fmtRfc3339 } from '@/lib/format'
import { usePollInterval } from '@/lib/sse'

export const Route = createFileRoute('/_app/history/')({
  component: HistoryPage,
})

const CHANGE_VERB: Record<string, string> = {
  deployed: 'Deployed',
  undeployed: 'Removed',
  changed: 'Changed',
}

function ChangeLine({ change }: { change: HistoryChange }) {
  const verb = CHANGE_VERB[change.change] ?? change.change
  const prep = change.change === 'undeployed' ? 'from' : 'to'
  return (
    <div className="flex items-center gap-2 text-sm">
      <span>{verb}</span>
      <span className="font-medium">{change.app}</span>
      <span className="text-muted-foreground">{prep}</span>
      <span>{change.scopeLabel}</span>
    </div>
  )
}

/** Expanded detail: a plain-language description of what a change touched. */
function EntryDetail({ id }: { id: number }) {
  const detail = useHistoryDetail(id)
  if (detail.data?.status !== 200) {
    return (
      <p className="px-4 pb-3 text-sm text-muted-foreground">
        {detail.isLoading ? 'Loading…' : 'Could not load details.'}
      </p>
    )
  }
  const d = detail.data.data
  return (
    <div className="flex flex-col gap-2 border-t px-4 py-3">
      {d.changes.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          A configuration change with no app-level deploys.
        </p>
      ) : (
        d.changes.map((c, i) => <ChangeLine key={i} change={c} />)
      )}
      {d.otherChanges > 0 && (
        <p className="text-xs text-muted-foreground">
          +{d.otherChanges} more change{d.otherChanges === 1 ? '' : 's'}.
        </p>
      )}
    </div>
  )
}

/**
 * History (§11.5): who changed what, when — presented as a plain
 * timeline. Each entry expands to a plain-language summary and can be
 * undone (which quietly restores the prior configuration as a new
 * change). No revision ids, layers, or diffs in operator copy.
 */
function HistoryPage() {
  const qc = useQueryClient()
  const refetchInterval = usePollInterval(30_000)
  const history = useHistoryList(
    { limit: 200 },
    { query: { refetchInterval } },
  )
  const me = useMe()
  const role = me.data?.status === 200 ? me.data.data.effectiveRole : undefined
  const operator = role === 'admin' || role === 'operator'
  const undo = useUndo()

  const rows = history.data?.status === 200 ? history.data.data : []
  const [expanded, setExpanded] = useState<Set<number>>(new Set())
  const [undoError, setUndoError] = useState<string | null>(null)

  const toggle = (id: number) =>
    setExpanded((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })

  const runUndo = async (id: number) => {
    setUndoError(null)
    const res = await undo.mutateAsync({ id })
    if (res.status === 200) {
      void qc.invalidateQueries({ queryKey: getHistoryListQueryKey() })
      void qc.invalidateQueries({ queryKey: getListQueryKey() })
    } else {
      const body = res.data
      setUndoError(
        body && typeof body === 'object' && 'error' in body
          ? String((body as { error: unknown }).error)
          : `Could not undo (HTTP ${res.status}).`,
      )
    }
  }

  return (
    <div className="flex flex-col gap-4 p-6">
      <h1 className="text-xl font-semibold tracking-tight">History</h1>
      {undoError && <p className="text-sm text-destructive">{undoError}</p>}

      <Card>
        <CardContent className="flex flex-col divide-y p-0">
          {rows.length === 0 ? (
            <p className="p-6 text-center text-sm text-muted-foreground">
              {history.isLoading ? 'Loading…' : 'Nothing has changed yet.'}
            </p>
          ) : (
            rows.map((e, i) => {
              const isOpen = expanded.has(e.id)
              return (
                <div key={e.id}>
                  <div className="flex items-center gap-3 px-4 py-3">
                    <button
                      type="button"
                      onClick={() => toggle(e.id)}
                      className="flex flex-1 items-center gap-3 text-left"
                    >
                      {isOpen ? (
                        <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
                      ) : (
                        <ChevronRight className="size-4 shrink-0 text-muted-foreground" />
                      )}
                      <span className="flex flex-1 flex-col">
                        <span className="text-sm font-medium">{e.summary}</span>
                        <span className="text-xs text-muted-foreground">
                          {e.who} · {fmtRfc3339(e.when)}
                        </span>
                      </span>
                    </button>
                    {i === 0 && (
                      <Badge variant="secondary" className="font-normal">
                        latest
                      </Badge>
                    )}
                    {operator && (
                      <AlertDialog>
                        <AlertDialogTrigger asChild>
                          <Button
                            variant="outline"
                            size="sm"
                            disabled={undo.isPending}
                          >
                            <Undo2 className="size-4" />
                            Undo
                          </Button>
                        </AlertDialogTrigger>
                        <AlertDialogContent>
                          <AlertDialogHeader>
                            <AlertDialogTitle>Undo this change?</AlertDialogTitle>
                            <AlertDialogDescription>
                              This restores the configuration to how it was
                              before "{e.summary}", as a new change on top.
                              {i !== 0 &&
                                ' Because later changes exist, undoing this one also reverts everything after it.'}
                            </AlertDialogDescription>
                          </AlertDialogHeader>
                          <AlertDialogFooter>
                            <AlertDialogCancel>Cancel</AlertDialogCancel>
                            <AlertDialogAction onClick={() => void runUndo(e.id)}>
                              Undo
                            </AlertDialogAction>
                          </AlertDialogFooter>
                        </AlertDialogContent>
                      </AlertDialog>
                    )}
                  </div>
                  <div className={cn(!isOpen && 'hidden')}>
                    {isOpen && <EntryDetail id={e.id} />}
                  </div>
                </div>
              )
            })
          )}
        </CardContent>
      </Card>
    </div>
  )
}
