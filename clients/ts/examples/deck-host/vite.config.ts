import {defaultClientConditions, defineConfig} from 'vite';

/** Same origin as the React example: `/v1/*` to the viewer plane, `/token` and `/users` to the app server. */
export default defineConfig({
  // The workspace packages resolve to their sources, so the page runs without a library build.
  resolve: {conditions: ['tessera-source', ...defaultClientConditions]},
  server: {
    port: 5183,
    strictPort: true,
    proxy: {
      '/v1': {target: process.env.TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585', changeOrigin: true},
      '/token': {target: process.env.TESSERA_APP_URL ?? 'http://127.0.0.1:5180', changeOrigin: true},
      '/users': {target: process.env.TESSERA_APP_URL ?? 'http://127.0.0.1:5180', changeOrigin: true}
    }
  }
});
