import {defaultClientConditions, defineConfig} from 'vite';
import {tesseraDecorators} from './vite-plugin-decorators.js';

/**
 * The self-contained bundle: one minified ESM file with lit and deck.gl inside it and the decode
 * worker inlined (`src/bundle.ts`). The npm distribution is the `tsc` build in `dist/` beside it,
 * with those as dependencies and peers.
 */
export default defineConfig({
  plugins: [tesseraDecorators()],
  // The workspace packages resolve to their sources, so the bundle needs no prior library build.
  resolve: {conditions: ['tessera-source', ...defaultClientConditions]},
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
    // The library build sits in the same directory.
    emptyOutDir: false,
    rolldownOptions: {
      output: {
        // One file: no chunks, no separate worker asset.
        codeSplitting: false,
        // Library mode keeps whitespace in an ES build so that a consumer's bundler can still
        // tree-shake it. Nothing bundles this file again, so it is minified whole. Licence
        // comments stay.
        minify: true,
        comments: {legal: true, annotation: false, jsdoc: false}
      }
    },
    sourcemap: false,
    minify: 'oxc',
    target: 'es2022'
  },
  worker: {format: 'es'}
});
