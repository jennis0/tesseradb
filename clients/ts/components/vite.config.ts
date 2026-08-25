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
  // page bundled by a host is fine (its bundler substitutes); this file is evaluated as-is — from
  // a `<script type="module">` and, as the widget's `_esm`, from a Blob URL inside JupyterLab,
  // where the first `process` is a ReferenceError before any element defines.
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
