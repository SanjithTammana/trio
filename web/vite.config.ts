import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

// Built into web/dist, which the Rust binary embeds. Dev server proxies the API to a running `trio open`.
export default defineConfig({
  plugins: [react()],
  build: { outDir: 'dist', emptyOutDir: true, sourcemap: false, chunkSizeWarningLimit: 900 },
  server: { proxy: { '/api': 'http://127.0.0.1:8964' } },
});
