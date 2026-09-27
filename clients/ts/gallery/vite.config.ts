import {defaultClientConditions, defineConfig} from 'vite';
import {tesseraDecorators} from '../components/vite-plugin-decorators.js';

/**
 * The gallery page, served from the workspace sources: the elements and the fake store the
 * component tests use, with no library build and no server.
 */
export default defineConfig({
  plugins: [tesseraDecorators()],
  resolve: {conditions: ['tessera-source', ...defaultClientConditions]},
  server: {port: Number(process.env.GALLERY_PORT ?? 5180)}
});
