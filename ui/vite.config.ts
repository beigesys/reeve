import path from 'node:path'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { tanstackRouter } from '@tanstack/router-plugin/vite'

// https://vite.dev/config/
export default defineConfig({
  plugins: [
    // Router plugin must run before react() (file-based route
    // generation, ui/src/routes/ -> src/route-tree.gen.ts).
    tanstackRouter({
      target: 'react',
      routesDirectory: './src/routes',
      generatedRouteTree: './src/route-tree.gen.ts',
      autoCodeSplitting: true,
    }),
    react(),
    tailwindcss(),
  ],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    // Dev mode (CLAUDE.md ui/, charter D3): vite proxies API traffic
    // to a running reeve-server; the generated client uses relative
    // URLs, so this is what makes `npm run dev` functional.
    proxy: {
      '/api': {
        target: 'http://localhost:8420',
        changeOrigin: true,
        // /api/reeve/v1/terminal/{device_id} is a websocket.
        ws: true,
      },
      '/v2': {
        target: 'http://localhost:8420',
        changeOrigin: true,
      },
    },
  },
})
