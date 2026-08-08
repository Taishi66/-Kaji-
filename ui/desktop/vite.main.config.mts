import { defineConfig } from 'vite';

// https://vitejs.dev/config
export default defineConfig({
  define: {
    'process.env.GITHUB_OWNER': JSON.stringify(process.env.GITHUB_OWNER || 'aaif-kaji'),
    'process.env.GITHUB_REPO': JSON.stringify(process.env.GITHUB_REPO || 'kaji'),
    'process.env.KAJI_BUNDLE_NAME': JSON.stringify(process.env.KAJI_BUNDLE_NAME || 'Kaji'),
  },
});
