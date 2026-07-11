// Tree-view helpers built ONLY on the generated client (fetchers +
// query-key factories from ui/src/api — D10; no hand-written API
// types). The revision-content layout is D11: `layers/**` +
// `packages/**`.
import { useQuery } from '@tanstack/react-query'
import {
  fileAt,
  getFileAtQueryKey,
  getGetRevisionQueryOptions,
  useGetRevision,
  useListRevisions,
} from '@/api/endpoints/tree/tree'
import type { RevisionInfo } from '@/api/model'
import { bytesToBase64, bytesToText } from '@/lib/base64'
import { usePollInterval } from '@/lib/sse'

/** Newest revision per stream from one history read (newest-first). */
export function useHeads(): {
  local?: RevisionInfo
  upstream?: RevisionInfo
  isLoading: boolean
  error: boolean
} {
  const refetchInterval = usePollInterval(30_000)
  const revs = useListRevisions({ limit: 1000 }, { query: { refetchInterval } })
  if (revs.data?.status !== 200) {
    return { isLoading: revs.isLoading, error: !revs.isLoading && !!revs.data }
  }
  const list = revs.data.data
  return {
    local: list.find((r) => r.stream === 'local'),
    upstream: list.find((r) => r.stream === 'upstream'),
    isLoading: false,
    error: false,
  }
}

/**
 * The effective head manifest: upstream head files overlaid by local
 * head files (authoring targets local; federation delivers upstream).
 * Values are blob digests; `streams` records who contributed a path.
 */
export function useHeadFiles(): {
  files: Record<string, string> | undefined
  streamOf: (path: string) => 'local' | 'upstream'
  local?: RevisionInfo
  upstream?: RevisionInfo
  isLoading: boolean
} {
  const heads = useHeads()
  const local = useGetRevision(heads.local?.id ?? -1, {
    query: { enabled: heads.local != null },
  })
  const upstream = useGetRevision(heads.upstream?.id ?? -1, {
    query: { enabled: heads.upstream != null },
  })

  const isLoading =
    heads.isLoading ||
    (heads.local != null && local.isLoading) ||
    (heads.upstream != null && upstream.isLoading)

  const localFiles =
    heads.local != null && local.data?.status === 200
      ? local.data.data.files
      : {}
  const upstreamFiles =
    heads.upstream != null && upstream.data?.status === 200
      ? upstream.data.data.files
      : {}

  const files = isLoading ? undefined : { ...upstreamFiles, ...localFiles }
  return {
    files,
    streamOf: (path) => (path in localFiles ? 'local' : 'upstream'),
    local: heads.local,
    upstream: heads.upstream,
    isLoading,
  }
}

/** Files of ONE revision (undefined while loading / on error). */
export function useRevisionFiles(id: number | undefined) {
  const q = useGetRevision(id ?? -1, { query: { enabled: id != null } })
  return q.data?.status === 200 ? q.data.data : undefined
}

/** Re-exported so pages can prefetch revision details imperatively. */
export { getGetRevisionQueryOptions }

/** Group a manifest's `layers/<layer>/**` paths by layer dir name. */
export function groupLayers(
  files: Record<string, string>,
): Map<string, string[]> {
  const groups = new Map<string, string[]>()
  for (const path of Object.keys(files).sort()) {
    const m = /^layers\/([^/]+)\/(.+)$/.exec(path)
    if (!m) continue
    const list = groups.get(m[1]) ?? []
    list.push(m[2])
    groups.set(m[1], list)
  }
  return groups
}

/** Group `packages/<name>/<version>/**` paths by (name, version). */
export function groupPackages(
  files: Record<string, string>,
): Map<string, { name: string; version: string; files: string[] }> {
  const groups = new Map<string, { name: string; version: string; files: string[] }>()
  for (const path of Object.keys(files).sort()) {
    const m = /^packages\/([^/]+)\/([^/]+)\/(.+)$/.exec(path)
    if (!m) continue
    const key = `${m[1]}/${m[2]}`
    const entry = groups.get(key) ?? { name: m[1], version: m[2], files: [] }
    entry.files.push(m[3])
    groups.set(key, entry)
  }
  return groups
}

export interface FileContent {
  /** UTF-8 text, or null when the blob is binary. */
  text: string | null
  base64: string
  size: number
}

async function fetchFileContent(id: number, path: string): Promise<FileContent> {
  const res = await fileAt(id, path)
  if (res.status !== 200) throw new Error(`file fetch failed (HTTP ${res.status})`)
  const bytes = new Uint8Array(await (res.data as Blob).arrayBuffer())
  return { text: bytesToText(bytes), base64: bytesToBase64(bytes), size: bytes.length }
}

/**
 * File content at a revision, decoded. Uses the GENERATED key factory
 * and fetcher; only the blob->text decode is local presentation.
 */
export function useFileContent(id: number | undefined, path: string | undefined) {
  return useQuery({
    queryKey:
      id != null && path != null
        ? [...getFileAtQueryKey(id, path), 'decoded']
        : ['file-at', 'disabled'],
    enabled: id != null && path != null,
    // Revision content is immutable — cache hard.
    staleTime: Infinity,
    queryFn: () => fetchFileContent(id as number, path as string),
  })
}

/** Imperative variant for bulk loads (edit page staging, reverts). */
export async function loadFileContent(id: number, path: string): Promise<FileContent> {
  return fetchFileContent(id, path)
}
