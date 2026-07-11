// Presentation-only formatting helpers (no API types live here —
// wire types come exclusively from ui/src/api, CLAUDE.md ui/ rule).

/** Unix seconds -> locale date-time string; em dash for null/absent. */
export function fmtUnix(secs: number | null | undefined): string {
  if (secs == null) return '—'
  return new Date(secs * 1000).toLocaleString()
}

/** RFC 3339 string -> locale date-time string. */
export function fmtRfc3339(ts: string | null | undefined): string {
  if (!ts) return '—'
  const d = new Date(ts)
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString()
}

/** Unix seconds -> compact "how long ago" ("42s", "3m", "5h", "2d"). */
export function fmtAgo(secs: number | null | undefined): string {
  if (secs == null) return 'never'
  const delta = Math.max(0, Math.floor(Date.now() / 1000) - secs)
  if (delta < 60) return `${delta}s`
  if (delta < 3600) return `${Math.floor(delta / 60)}m`
  if (delta < 86400) return `${Math.floor(delta / 3600)}h`
  return `${Math.floor(delta / 86400)}d`
}

/** Shorten a digest ("sha256:abcd…" -> "sha256:abcdef012345…"). */
export function fmtDigest(digest: string | null | undefined): string {
  if (!digest) return '—'
  const [algo, hex] = digest.includes(':')
    ? (digest.split(':', 2) as [string, string])
    : ['', digest]
  const short = hex.length > 12 ? `${hex.slice(0, 12)}…` : hex
  return algo ? `${algo}:${short}` : short
}
