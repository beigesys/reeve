import { useEffect, useState } from 'react'
import { Link, createFileRoute, useNavigate } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { ArrowLeft, Plus, Trash2 } from 'lucide-react'
import { usePutLayer } from '@/api/endpoints/tree/tree'
import { Badge } from '@/components/ui/badge'
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
import { Textarea } from '@/components/ui/textarea'
import { cn } from '@/lib/utils'
import { textToBase64 } from '@/lib/base64'
import { loadFileContent, useHeadFiles } from '@/lib/tree'

export const Route = createFileRoute('/_app/tree/layers/$layer/edit')({
  component: LayerEditPage,
})

interface StagedFile {
  /** Editable text, or null for binary passthrough content. */
  text: string | null
  /** Original bytes for binary files (sent unchanged). */
  base64: string
  origin: 'head' | 'new'
  dirty: boolean
}

/**
 * Staged working set for one layer. The PUT is batch-declarative
 * (docs/decisions/tree-render.md D14): the request replaces the WHOLE
 * layer, so deleting a file here really deletes it on apply.
 */
function LayerEditPage() {
  const { layer } = Route.useParams()
  const { files, streamOf, local, upstream, isLoading } = useHeadFiles()
  const navigate = useNavigate()
  const qc = useQueryClient()
  const put = usePutLayer()

  const [staged, setStaged] = useState<Map<string, StagedFile> | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [selected, setSelected] = useState<string | null>(null)
  const [newPath, setNewPath] = useState('')
  const [message, setMessage] = useState('')
  const [applyError, setApplyError] = useState<string | null>(null)

  const prefix = `layers/${layer}/`

  // Pre-populate the staged set from head content, once per page load.
  useEffect(() => {
    if (files == null || staged != null) return
    const layerPaths = Object.keys(files)
      .filter((p) => p.startsWith(prefix))
      .sort()
    let cancelled = false
    void (async () => {
      try {
        const next = new Map<string, StagedFile>()
        for (const path of layerPaths) {
          const revId = streamOf(path) === 'local' ? local?.id : upstream?.id
          if (revId == null) continue
          const content = await loadFileContent(revId, path)
          next.set(path.slice(prefix.length), {
            text: content.text,
            base64: content.base64,
            origin: 'head',
            dirty: false,
          })
        }
        if (!cancelled) setStaged(next)
      } catch (e) {
        if (!cancelled)
          setLoadError(e instanceof Error ? e.message : 'load failed')
      }
    })()
    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [files == null, layer])

  const rels = staged ? [...staged.keys()].sort() : []
  const selectedRel = selected != null && staged?.has(selected) ? selected : rels[0] ?? null
  const selectedFile = selectedRel != null ? staged?.get(selectedRel) : undefined

  const addFile = () => {
    const rel = newPath.trim().replace(/^\/+/, '')
    if (!rel || staged == null || staged.has(rel)) return
    const next = new Map(staged)
    next.set(rel, { text: '', base64: '', origin: 'new', dirty: true })
    setStaged(next)
    setSelected(rel)
    setNewPath('')
  }

  const removeFile = (rel: string) => {
    if (staged == null) return
    const next = new Map(staged)
    next.delete(rel)
    setStaged(next)
    if (selected === rel) setSelected(null)
  }

  const editFile = (rel: string, text: string) => {
    if (staged == null) return
    const prev = staged.get(rel)
    if (!prev) return
    const next = new Map(staged)
    next.set(rel, { ...prev, text, dirty: true })
    setStaged(next)
  }

  const apply = async () => {
    if (staged == null) return
    setApplyError(null)
    const body: Record<string, string> = {}
    for (const [rel, f] of staged) {
      body[rel] = f.text != null ? textToBase64(f.text) : f.base64
    }
    const res = await put.mutateAsync({
      layer,
      data: { files: body, message: message.trim() || null },
    })
    if (res.status === 200) {
      // A commit moves head — every tree-derived view is stale.
      void qc.invalidateQueries()
      void navigate({ to: '/tree/layers/$layer', params: { layer } })
    } else {
      const detail =
        res.status === 422 && res.data && 'error' in res.data
          ? String(res.data.error)
          : `HTTP ${res.status}`
      setApplyError(detail)
    }
  }

  const dirty = staged != null && [...staged.values()].some((f) => f.dirty)
  const removedAny =
    files != null &&
    staged != null &&
    Object.keys(files).filter((p) => p.startsWith(prefix)).length > staged.size

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/tree/layers/$layer" params={{ layer }}>
            <ArrowLeft className="size-4" />
            {layer}
          </Link>
        </Button>
        <h1 className="font-mono text-xl font-semibold tracking-tight">
          Edit {layer}
        </h1>
      </div>

      {loadError != null ? (
        <p className="text-sm text-destructive">{loadError}</p>
      ) : isLoading || staged == null ? (
        <p className="text-sm text-muted-foreground">Loading layer content…</p>
      ) : (
        <>
          <div className="flex gap-4">
            <Card className="w-72 shrink-0">
              <CardHeader>
                <CardTitle className="text-base">Staged files</CardTitle>
                <CardDescription>
                  Applying replaces the whole layer with this set.
                </CardDescription>
              </CardHeader>
              <CardContent className="flex flex-col gap-2 p-2">
                <div className="flex flex-col gap-0.5">
                  {rels.length === 0 && (
                    <p className="px-2 py-1 text-sm text-muted-foreground">
                      Empty layer (applying deletes every file).
                    </p>
                  )}
                  {rels.map((rel) => {
                    const f = staged.get(rel) as StagedFile
                    return (
                      <div key={rel} className="flex items-center gap-1">
                        <button
                          type="button"
                          onClick={() => setSelected(rel)}
                          className={cn(
                            'min-w-0 flex-1 truncate rounded px-2 py-1 text-left font-mono text-xs hover:bg-accent',
                            rel === selectedRel && 'bg-accent font-medium',
                          )}
                        >
                          {rel}
                          {f.dirty && <span className="text-amber-500"> *</span>}
                        </button>
                        <Button
                          variant="ghost"
                          size="sm"
                          className="size-7 p-0 text-muted-foreground"
                          onClick={() => removeFile(rel)}
                          aria-label={`Delete ${rel}`}
                        >
                          <Trash2 className="size-3.5" />
                        </Button>
                      </div>
                    )
                  })}
                </div>
                <form
                  className="flex items-center gap-1 border-t pt-2"
                  onSubmit={(e) => {
                    e.preventDefault()
                    addFile()
                  }}
                >
                  <Input
                    placeholder="compose/app.yaml"
                    value={newPath}
                    onChange={(e) => setNewPath(e.target.value)}
                    className="h-8 font-mono text-xs"
                  />
                  <Button
                    type="submit"
                    variant="outline"
                    size="sm"
                    disabled={!newPath.trim()}
                  >
                    <Plus className="size-4" />
                  </Button>
                </form>
              </CardContent>
            </Card>

            <Card className="min-w-0 flex-1">
              <CardHeader>
                <CardTitle className="flex items-center gap-2 font-mono text-base">
                  {selectedRel ?? 'No file selected'}
                  {selectedFile?.origin === 'new' && (
                    <Badge variant="secondary" className="font-normal">
                      new
                    </Badge>
                  )}
                </CardTitle>
              </CardHeader>
              <CardContent>
                {selectedRel == null || selectedFile == null ? (
                  <p className="text-sm text-muted-foreground">
                    Select or add a file.
                  </p>
                ) : selectedFile.text == null ? (
                  <p className="text-sm text-muted-foreground">
                    Binary file — kept byte-identical on apply.
                  </p>
                ) : (
                  <Textarea
                    value={selectedFile.text}
                    onChange={(e) => editFile(selectedRel, e.target.value)}
                    spellCheck={false}
                    className="min-h-[50vh] font-mono text-xs"
                  />
                )}
              </CardContent>
            </Card>
          </div>

          <Card>
            <CardHeader>
              <CardTitle className="text-base">Apply</CardTitle>
              <CardDescription>
                One commit: the staged set becomes the layer's complete
                content in a new local revision (idempotent — identical
                content produces no revision).
              </CardDescription>
            </CardHeader>
            <CardContent className="flex flex-col gap-3">
              <div className="flex max-w-xl flex-col gap-1.5">
                <Label htmlFor="commit-message">Commit message</Label>
                <Input
                  id="commit-message"
                  placeholder={`update ${layer}`}
                  value={message}
                  onChange={(e) => setMessage(e.target.value)}
                />
              </div>
              <div className="flex items-center gap-3">
                <Button
                  onClick={() => void apply()}
                  disabled={put.isPending || (!dirty && !removedAny)}
                >
                  {put.isPending ? 'Applying…' : 'Apply as new revision'}
                </Button>
                {!dirty && !removedAny && (
                  <span className="text-xs text-muted-foreground">
                    No staged changes.
                  </span>
                )}
                {applyError && (
                  <span className="text-sm text-destructive">{applyError}</span>
                )}
              </div>
            </CardContent>
          </Card>
        </>
      )}
    </div>
  )
}
