import react from '@vitejs/plugin-react';
import { readFileSync } from 'node:fs';
import { defineConfig, type Plugin } from 'vite';

// Third-party notices (the Astryx MIT text) ship inside the embedded UI as a plain file, because
// the minifier drops license comments from the bundle.
const notices: Plugin = {
  name: 'trio-notices',
  generateBundle() {
    this.emitFile({ type: 'asset', fileName: 'THIRD_PARTY_NOTICES.txt', source: readFileSync(new URL('../THIRD_PARTY_NOTICES.md', import.meta.url), 'utf8') });
  },
};

// Built into web/dist, which the Rust binary embeds. Dev server proxies the API to a running `trio open`.
export default defineConfig({
  plugins: [react(), notices],
  build: { outDir: 'dist', emptyOutDir: true, sourcemap: false, chunkSizeWarningLimit: 900 },
  server: { proxy: { '/api': 'http://127.0.0.1:8964' } },
});
