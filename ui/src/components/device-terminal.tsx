// Remote terminal, UI leg (spec/reeve/03-terminal.md §5; CLAUDE.md
// guardrails): sessions are short-lived and explicitly initiated —
// connect happens on click, never automatically, and a dropped
// session is NOT auto-reopened (§5.3: reconnection is a new session).
//
// Wire framing (crates/reeve-types src/reeve/terminal.rs — the
// agent-owned in-band encoding the bridge relays opaquely, §5.5):
//   byte 0 = 0x00 -> raw terminal bytes (both directions)
//   byte 0 = 0x01 -> resize, body u16 BE cols + u16 BE rows (UI->agent)
import { useCallback, useEffect, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { getTerminalRouteUrl } from '@/api/endpoints/terminal/terminal'
import { Button } from '@/components/ui/button'

const FRAME_DATA = 0
const FRAME_RESIZE = 1

function encodeData(text: string): Uint8Array<ArrayBuffer> {
  const bytes = new TextEncoder().encode(text)
  const frame = new Uint8Array(bytes.length + 1)
  frame[0] = FRAME_DATA
  frame.set(bytes, 1)
  return frame
}

function encodeResize(cols: number, rows: number): Uint8Array<ArrayBuffer> {
  return new Uint8Array([
    FRAME_RESIZE,
    (cols >> 8) & 0xff,
    cols & 0xff,
    (rows >> 8) & 0xff,
    rows & 0xff,
  ])
}

type Phase = 'idle' | 'connecting' | 'open' | 'closed'

export function DeviceTerminal({
  deviceId,
  online,
  operator,
}: {
  deviceId: string
  online: boolean
  /** Whether the current identity acts as operator+ (§5.6). */
  operator: boolean
}) {
  const containerRef = useRef<HTMLDivElement>(null)
  const wsRef = useRef<WebSocket | null>(null)
  const termRef = useRef<Terminal | null>(null)
  const [phase, setPhase] = useState<Phase>('idle')
  const [closeReason, setCloseReason] = useState<string | null>(null)

  const teardown = useCallback(() => {
    wsRef.current?.close()
    wsRef.current = null
    termRef.current?.dispose()
    termRef.current = null
  }, [])

  // Component unmount ends the session (§5.3: no background sessions).
  useEffect(() => teardown, [teardown])

  const connect = useCallback(() => {
    const container = containerRef.current
    if (!container || wsRef.current) return
    // A fresh connect is a NEW session (§5.3); drop any finished
    // terminal still showing the previous session's output.
    termRef.current?.dispose()
    termRef.current = null
    setCloseReason(null)
    setPhase('connecting')

    const term = new Terminal({
      fontSize: 13,
      fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
      cursorBlink: true,
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(container)
    fit.fit()
    termRef.current = term

    // Generated URL builder (D10) — then swapped to the ws scheme.
    const url = new URL(
      getTerminalRouteUrl(deviceId, {
        cols: term.cols,
        rows: term.rows,
        term: 'xterm-256color',
      }),
      window.location.href,
    )
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:'

    const ws = new WebSocket(url)
    ws.binaryType = 'arraybuffer'
    wsRef.current = ws

    ws.onopen = () => {
      setPhase('open')
      term.focus()
    }
    ws.onmessage = (ev: MessageEvent) => {
      if (!(ev.data instanceof ArrayBuffer)) return
      const payload = new Uint8Array(ev.data)
      // Tolerant reader: ignore unknown discriminators.
      if (payload.length > 0 && payload[0] === FRAME_DATA) {
        term.write(payload.subarray(1))
      }
    }
    ws.onclose = (ev: CloseEvent) => {
      // Pre-upgrade denials (403 not enabled / 409 offline …) surface
      // as an opaque failure; post-upgrade denials carry the bridge's
      // close reason (code 1011, e.g. "not enabled").
      setCloseReason(
        ev.reason ||
          (ev.code === 1000 || ev.code === 1005
            ? null
            : 'connection rejected — device offline, terminal not enabled in desired state, or operator role required'),
      )
      setPhase('closed')
      wsRef.current = null
    }

    term.onData((data) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(encodeData(data))
    })
    term.onResize(({ cols, rows }) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(encodeResize(cols, rows))
    })

    const observer = new ResizeObserver(() => {
      if (termRef.current === term) fit.fit()
    })
    observer.observe(container)
    ws.addEventListener('close', () => observer.disconnect())
  }, [deviceId])

  const disconnect = useCallback(() => {
    teardown()
    setPhase('idle')
  }, [teardown])

  const live = phase === 'open' || phase === 'connecting'
  const blocked = !online || !operator

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-3">
        {live ? (
          <Button variant="destructive" size="sm" onClick={disconnect}>
            Disconnect
          </Button>
        ) : (
          <Button size="sm" onClick={connect} disabled={blocked}>
            Connect
          </Button>
        )}
        <span className="text-sm text-muted-foreground">
          {!online
            ? 'device offline — the terminal needs a live channel (no queueing)'
            : !operator
              ? 'operator role required to open a terminal session'
              : phase === 'connecting'
                ? 'connecting…'
                : phase === 'open'
                  ? 'session open (audited)'
                  : phase === 'closed'
                    ? 'session ended'
                    : 'sessions are short-lived, explicitly initiated, and audited'}
        </span>
      </div>
      {closeReason && (
        <p className="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm text-amber-700 dark:text-amber-400">
          {closeReason}
        </p>
      )}
      <div
        ref={containerRef}
        className="h-[28rem] overflow-hidden rounded-md border bg-black p-2"
      />
    </div>
  )
}
