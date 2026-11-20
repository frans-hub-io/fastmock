import { readFileSync } from 'node:fs';
import { defineConfig } from 'vitest/config';
import type { Plugin } from 'vite';
import react from '@vitejs/plugin-react';

const admin = process.env.FASTMOCK_ADMIN_URL ?? 'http://127.0.0.1:25090';

/** Ships the OFL texts of the bundled fonts next to the font files in `dist/assets/`. */
function fontLicenses(): Plugin {
  return {
    name: 'fastmock-font-licenses',
    apply: 'build',
    generateBundle() {
      for (const name of ['OFL-NotoSans.txt', 'OFL-JetBrainsMono.txt']) {
        this.emitFile({ type: 'asset', fileName: `assets/${name}`, source: readFileSync(new URL(`./src/assets/fonts/${name}`, import.meta.url)) });
      }
    },
  };
}

export default defineConfig({
  base: '/ui/',
  plugins: [react(), fontLicenses()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    assetsInlineLimit: 0,
    chunkSizeWarningLimit: 2500,
    rolldownOptions: {
      output: {
        manualChunks(id: string) {
          if (id.includes('node_modules/swagger-ui-dist')) return 'swagger';
          if (id.includes('node_modules/@codemirror') || id.includes('node_modules/@lezer')) return 'codemirror';
          if (id.includes('node_modules/@mantine') || id.includes('node_modules/@floating-ui')) return 'mantine';
          if (id.includes('node_modules/react') || id.includes('node_modules/scheduler')) return 'react';
          return undefined;
        },
      },
    },
  },
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      '/__admin': { target: admin, changeOrigin: false },
      '/openapi.json': { target: admin, changeOrigin: false },
      '/metrics': { target: admin, changeOrigin: false },
      '/ui/config.json': { target: admin, changeOrigin: false },
    },
  },
  test: {
    environment: 'jsdom',
    globals: false,
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.{ts,tsx}'],
    css: false,
    testTimeout: 30000,
  },
});
