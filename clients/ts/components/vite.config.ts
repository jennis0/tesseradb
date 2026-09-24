import {defineConfig} from 'vite';
import {tesseraDecorators} from './vite-plugin-decorators.js';

/**
 * The self-contained bundle: one ESM file, lit and deck.gl inside it, the decode worker inlined
 * (`src/bundle.ts`). The unbundled distribution is the source itself, with those as peers; a
 * host with a bundler resolves `@tesseradb/components` to `src/index.ts`.
 */
export default defineConfig({
  plugins: [tesseraDecorators()],
  // deck.gl reads `process.env.NODE_ENV` unguarded, and library mode does not substitute it. A
  // host's bundler substitutes it, but this bundle is also loaded as-is from a
  // `<script type="module">` and, as the widget's `_esm`, from a Blob URL in JupyterLab, where
  // `process` is undefined.
  define: {'process.env.NODE_ENV': JSON.stringify('production')},
  build: {
    lib: {
      entry: 'src/bundle.ts',
      formats: ['es'],
      fileName: () => 'tessera-components.js'
    },
    rollupOptions: {
      output: {
        // One file: no chunks, no separate worker asset.
        inlineDynamicImports: true,
        manualChunks: undefined
      }
    },
    sourcemap: false,
    minify: 'esbuild',
    target: 'es2022'
  },
  worker: {format: 'es'}
});
