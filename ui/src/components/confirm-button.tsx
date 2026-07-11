import { useEffect, useState } from 'react'
import { Button } from '@/components/ui/button'

/**
 * Two-step inline confirm for destructive actions (delete/revoke).
 * First click arms it ("Confirm?" for a few seconds), second click
 * fires. No modal — CLAUDE.md forbids CRUD modals.
 */
export function ConfirmButton({
  label,
  confirmLabel = 'Confirm?',
  onConfirm,
  disabled,
  size = 'sm',
}: {
  label: string
  confirmLabel?: string
  onConfirm: () => void
  disabled?: boolean
  size?: 'sm' | 'default'
}) {
  const [armed, setArmed] = useState(false)

  useEffect(() => {
    if (!armed) return
    const t = setTimeout(() => setArmed(false), 4000)
    return () => clearTimeout(t)
  }, [armed])

  return (
    <Button
      variant={armed ? 'destructive' : 'outline'}
      size={size}
      disabled={disabled}
      onClick={() => {
        if (armed) {
          setArmed(false)
          onConfirm()
        } else {
          setArmed(true)
        }
      }}
    >
      {armed ? confirmLabel : label}
    </Button>
  )
}
