import {defineConfig} from 'vite';

/** Same origin as the React example: `/v1/*` to the viewer plane, `/token` and `/users` to the app server. */
export default defineConfig({
  server: {
    port: 5182,
    strictPort: true,
    proxy: {
      '/v1': {target: process.env.TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585', changeOrigin: true},
      '/token': {target: process.env.TESSERA_APP_URL ?? 'http://127.0.0.1:5180', changeOrigin: true},
      '/users': {target: process.env.TESSERA_APP_URL ?? 'http://127.0.0.1:5180', changeOrigin: true}
    }
  }
});
