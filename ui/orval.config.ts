// D10 API type pipeline (docs/decisions/ui.md): ui/openapi.json (from
// `just gen-api` -> `reeve-server openapi`) -> generated TanStack
// Query client under src/api/. That directory is GENERATED — never
// hand-edited; CI fails on drift (`just check-api-drift`).
import { defineConfig } from 'orval'

export default defineConfig({
  reeve: {
    input: './openapi.json',
    output: {
      // One directory per OpenAPI tag (tags are kebab-case in the
      // Rust annotations, so generated file names stay kebab-case —
      // CLAUDE.md ui/ rule).
      mode: 'tags-split',
      target: './src/api/endpoints',
      schemas: './src/api/model',
      client: 'react-query',
      httpClient: 'fetch',
      clean: true,
      // CLAUDE.md ui/ rule: file names ALWAYS kebab-case — including
      // generated ones.
      namingConvention: 'kebab-case',
      // Same-origin relative URLs: vite proxies /api in dev, the
      // reeve-server embed serves the UI in prod.
      baseUrl: '',
      // Orval's default react-query split: GETs -> useQuery hooks +
      // query-key factories (getXQueryKey — the ONLY query keys the
      // UI uses; SSE invalidation goes through these factories,
      // spec/reeve/04-status-stream.md §6); writes -> useMutation.
    },
  },
})
