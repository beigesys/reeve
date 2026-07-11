import { useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import {
  getGroupsListQueryKey,
  useGroupsCreate,
  useGroupsDelete,
  useGroupsRename,
} from '@/api/endpoints/groups/groups'
import type { GroupKind, GroupNode } from '@/api/model'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { ConfirmButton } from '@/components/confirm-button'

/** A best-effort human string out of an API error body. */
function errText(body: unknown, fallback: string): string {
  if (body && typeof body === 'object' && 'error' in body) {
    return String((body as { error: unknown }).error)
  }
  return fallback
}

type CreateProps = {
  mode: 'create'
  kind: GroupKind
  /** Containing fleet id (sites only). */
  parentId?: number
  parentName?: string
}
type RenameProps = { mode: 'rename'; group: GroupNode; parentName?: string }

/**
 * The one shared fleet/site group form (spec/reeve/11-fleet-model.md §11.3),
 * reused by the create and rename pages. Create: a fleet (top level) or a
 * site under a fleet. Rename/delete are refused (409) by the server while
 * the group is in use — the copy surfaces that.
 */
export function GroupForm(props: CreateProps | RenameProps) {
  const navigate = useNavigate()
  const qc = useQueryClient()
  const create = useGroupsCreate()
  const rename = useGroupsRename()
  const del = useGroupsDelete()

  const kind: GroupKind = props.mode === 'create' ? props.kind : props.group.kind
  const [name, setName] = useState(
    props.mode === 'rename' ? props.group.name : '',
  )
  const [error, setError] = useState<string | null>(null)

  const noun = kind === 'fleet' ? 'fleet' : 'site'
  const invalidate = () =>
    void qc.invalidateQueries({ queryKey: getGroupsListQueryKey() })
  const done = () => navigate({ to: '/fleet' })

  const submit = async () => {
    setError(null)
    const trimmed = name.trim()
    if (trimmed === '') {
      setError('Enter a name.')
      return
    }
    if (props.mode === 'create') {
      const res = await create.mutateAsync({
        data: {
          kind,
          name: trimmed,
          ...(kind === 'site' ? { parentId: props.parentId } : {}),
        },
      })
      if (res.status === 201) {
        invalidate()
        done()
      } else if (res.status === 409) {
        setError(
          errText(
            res.data,
            `A ${noun} named "${trimmed}" already exists${kind === 'site' ? ' in this fleet' : ''}.`,
          ),
        )
      } else {
        setError(errText(res.data, `Could not create ${noun} (HTTP ${res.status}).`))
      }
    } else {
      const res = await rename.mutateAsync({
        id: props.group.id,
        data: { name: trimmed },
      })
      if (res.status === 200) {
        invalidate()
        done()
      } else if (res.status === 409) {
        setError(
          errText(
            res.data,
            `Cannot rename: the ${noun} is in use (a device references it${kind === 'fleet' ? ', or it still has sites' : ''}), or the name is taken. Reassign first.`,
          ),
        )
      } else {
        setError(errText(res.data, `Could not rename (HTTP ${res.status}).`))
      }
    }
  }

  const doDelete = async () => {
    if (props.mode !== 'rename') return
    setError(null)
    const res = await del.mutateAsync({ id: props.group.id })
    if (res.status === 204) {
      invalidate()
      done()
    } else if (res.status === 409) {
      setError(
        errText(
          res.data,
          `Cannot delete: the ${noun} is in use (a device references it${kind === 'fleet' ? ', or it still has sites' : ''}). Reassign first.`,
        ),
      )
    } else {
      setError(errText(res.data, `Could not delete (HTTP ${res.status}).`))
    }
  }

  const pending = create.isPending || rename.isPending || del.isPending
  const parentName = props.parentName

  return (
    <div className="flex max-w-lg flex-col gap-5">
      {kind === 'site' && parentName && (
        <div className="flex flex-col gap-1.5">
          <Label>Fleet</Label>
          <Input value={parentName} disabled />
          <span className="text-xs text-muted-foreground">
            A site belongs to exactly one fleet.
          </span>
        </div>
      )}

      <div className="flex flex-col gap-1.5">
        <Label htmlFor="group-name">
          {kind === 'fleet' ? 'Fleet name' : 'Site name'}
        </Label>
        <Input
          id="group-name"
          value={name}
          autoFocus
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault()
              void submit()
            }
          }}
        />
      </div>

      {error && <p className="text-sm text-destructive">{error}</p>}

      <div className="flex items-center justify-between gap-3 border-t pt-4">
        <div className="flex items-center gap-2">
          <Button onClick={() => void submit()} disabled={pending}>
            {props.mode === 'create'
              ? `Create ${noun}`
              : pending
                ? 'Saving…'
                : 'Save name'}
          </Button>
          <Button variant="ghost" onClick={done}>
            Cancel
          </Button>
        </div>
        {props.mode === 'rename' && (
          <ConfirmButton
            label="Delete"
            confirmLabel={`Delete ${noun} "${props.group.name}"?`}
            description={`This removes the ${noun} from the location tree. It is refused while any device is still assigned to it${kind === 'fleet' ? ', or while it still has sites' : ''}.`}
            onConfirm={() => void doDelete()}
            disabled={pending}
          />
        )}
      </div>
    </div>
  )
}
