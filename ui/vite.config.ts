import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

const admin = process.env.FASTMOCK_ADMIN_URL ?? 'http://127.0.0.1:25090';

export default defineConfig({
  base: '/ui/',
  plugins: [react()],
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
