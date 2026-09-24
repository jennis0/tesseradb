import react from '@vitejs/plugin-react';
import {defaultClientConditions, defineConfig} from 'vite';
import {tesseraDecorators} from '../../components/vite-plugin-decorators.js';

/**
 * Same-origin, as the page will be in production: `/v1/*` is proxied to the viewer plane with
 * every response header kept (client-obligations rule 10), and `/token` and `/users` go to the
 * app server in `../plain-html/server.mjs`, which holds the session credential. The browser sees
 * one origin and no CORS is involved.
 */
export default defineConfig({
  plugins: [react(), tesseraDecorators()],
  // The workspace packages resolve to their sources, so the page runs without a library build.
  resolve: {conditions: ['tessera-source', ...defaultClientConditions]},
  server: {
    port: 5181,
    strictPort: true,
    proxy: {
      '/v1': {target: process.env.TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585', changeOrigin: true},
      '/token': {target: process.env.TESSERA_APP_URL ?? 'http://127.0.0.1:5180', changeOrigin: true},
      '/users': {target: process.env.TESSERA_APP_URL ?? 'http://127.0.0.1:5180', changeOrigin: true}
    }
  }
});
