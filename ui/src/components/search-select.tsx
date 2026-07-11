import { useMemo, useState } from 'react'
import { Plus } from 'lucide-react'

import {
  Combobox,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
} from '@/components/ui/combobox'

export type ComboboxOption = {
  value: string
  label: string
}

// Objects carried through base-ui: `value` is the wire value, `label` the
// display text, `create` marks the synthetic "add a new one" row.
type Item = { value: string; label: string; create?: boolean }

/**
 * Searchable single-select — a thin wrapper around the official shadcn/ui
 * (base-ui) Combobox primitives, exposing a small controlled API:
 * `value` in, `onChange` out, string `options`.
 *
 * - The input filters `options` as you type (base-ui default filter).
 * - `creatable`: when the typed query matches no option, an extra
 *   "Create '<query>'" row selects the typed value verbatim (free-add of
 *   new group names).
 * - `clearable`: renders the built-in clear (×) button; clearing emits ''.
 *
 * Primitives in @/components/ui are treated as read-only; all custom
 * behavior lives here.
 */
export function SearchSelect({
  value,
  onChange,
  options,
  placeholder = 'Select…',
  emptyText = 'No results.',
  creatable = false,
  clearable = false,
  disabled = false,
  className,
  id,
}: {
  value: string
  onChange: (value: string) => void
  options: ComboboxOption[]
  placeholder?: string
  /** @deprecated kept for call-site compatibility; base-ui uses one input. */
  searchPlaceholder?: string
  emptyText?: string
  creatable?: boolean
  clearable?: boolean
  disabled?: boolean
  className?: string
  id?: string
}) {
  const [query, setQuery] = useState('')

  const trimmed = query.trim()
  const exactMatch = options.some(
    (o) => o.value.toLowerCase() === trimmed.toLowerCase(),
  )

  const items: Item[] = useMemo(() => {
    const base: Item[] = options.map((o) => ({ value: o.value, label: o.label }))
    if (creatable && trimmed !== '' && !exactMatch) {
      base.push({ value: trimmed, label: trimmed, create: true })
    }
    return base
  }, [options, creatable, trimmed, exactMatch])

  // Controlled selection as an object so base-ui can render the label even
  // when the value is a freshly-created name not (yet) in `options`.
  const selected: Item | null =
    value === ''
      ? null
      : (options.find((o) => o.value === value) ?? { value, label: value })

  return (
    <Combobox<Item>
      items={items}
      value={selected}
      disabled={disabled}
      itemToStringLabel={(it) => it.label}
      itemToStringValue={(it) => it.value}
      isItemEqualToValue={(a, b) => a.value === b.value}
      // Input text is uncontrolled so base-ui shows the selected label on
      // mount (edit forms) and while closed; we only track what's typed so
      // the "create" row can react to the query.
      onInputValueChange={(next) => setQuery(next)}
      onValueChange={(next) => onChange(next ? next.value : '')}
    >
      <ComboboxInput
        id={id}
        placeholder={placeholder}
        showClear={clearable}
        className={className}
      />
      <ComboboxContent>
        <ComboboxEmpty>{emptyText}</ComboboxEmpty>
        <ComboboxList>
          {(item: Item) =>
            item.create ? (
              <ComboboxItem key={`__create__${item.value}`} value={item}>
                <Plus className="size-4" />
                Create &ldquo;{item.value}&rdquo;
              </ComboboxItem>
            ) : (
              <ComboboxItem key={item.value} value={item}>
                {item.label}
              </ComboboxItem>
            )
          }
        </ComboboxList>
      </ComboboxContent>
    </Combobox>
  )
}
