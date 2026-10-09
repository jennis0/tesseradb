import {defineConfig} from 'vitest/config';

/**
 * `@lit/react`'s `node` export condition is the SSR build, which sets no properties on the element
 * and leaves them to Lit's hydration. A page runs the `browser` build, so the tests resolve that.
 * `mosaica-source` resolves the workspace packages to their sources.
 */
export default defineConfig({
  resolve: {conditions: ['mosaica-source', 'browser']},
  test: {environment: 'happy-dom', include: ['test/**/*.test.ts', 'test/**/*.test.tsx']},
  esbuild: {jsx: 'automatic'}
});
