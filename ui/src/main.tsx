import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { RouterProvider, createRouter } from '@tanstack/react-router'
import { routeTree } from './route-tree.gen'
import './index.css'

// Dark-mode-capable neutral theme (CLAUDE.md ui/): the shadcn `dark`
// variant keys off a `dark` class on <html>; follow the OS preference.
const media = window.matchMedia('(prefers-color-scheme: dark)')
const applyTheme = () =>
  document.documentElement.classList.toggle('dark', media.matches)
applyTheme()
media.addEventListener('change', applyTheme)

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // The generated fetch client never throws on HTTP errors (it
      // returns { status, data }), so retries only cover network
      // failures — one is plenty for an offline-first fleet server.
      retry: 1,
      staleTime: 5_000,
    },
  },
})

const router = createRouter({
  routeTree,
  context: { queryClient },
  defaultPreload: 'intent',
})

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router
  }
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
)
