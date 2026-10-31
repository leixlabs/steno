import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The built bundle is copied to `server.web_dir` (default `web/dist`).
// During development `npm run dev` proxies the API to a running server, so the
// UI can be edited without rebuilding the Rust binary.
// tools/dev.sh sets this so `--port 8081` reaches the backend it actually
// started. Without it (plain `npm run dev`) the usual server on 8080 is assumed.
const apiTarget = process.env.VITE_API_TARGET ?? 'http://127.0.0.1:8080'

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    // Bind IPv4 explicitly. Vite's default (`localhost`) resolves to ::1 on
    // macOS, which makes http://127.0.0.1:5173 refuse connections — confusing
    // when everything else on this project is addressed by IP.
    host: '127.0.0.1',
    port: 5173,
    // A second dev server on a busy 5173 would silently proxy to the wrong
    // backend, so refuse instead of picking the next free port.
    strictPort: true,
    proxy: {
      // `ws: true` is what makes `npm run dev` usable for the live transcript:
      // without it the upgrade request is answered as plain HTTP and the socket
      // never opens on :5173.
      '/api': { target: apiTarget, ws: true },
    },
  },
})
