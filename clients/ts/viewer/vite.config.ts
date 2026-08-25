import {defineConfig} from 'vite';
import {tesseraDecorators} from '@tesseradb/components/vite-plugin-decorators';

export default defineConfig({
  plugins: [tesseraDecorators()],
  // strictPort, because the origin is enumerated in the server's `serve.dev_cors_origins`: a
  // silent fallback to 5174 would produce a CORS failure that reads as a broken server.
  server: {port: 5173, strictPort: true}
});
