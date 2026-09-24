import {defineConfig} from 'vitest/config';

/**
 * `@lit/react` ships a `node` export condition whose build sets no properties on the element: it
 * is the SSR variant, which hands them to Lit's hydration instead. A page runs the `browser`
 * build, so the tests resolve that one; without it every property test passes `null` through.
 * `tessera-source` resolves the workspace packages to their sources.
 */
export default defineConfig({
  resolve: {conditions: ['tessera-source', 'browser']},
  test: {environment: 'happy-dom', include: ['test/**/*.test.ts', 'test/**/*.test.tsx']},
  esbuild: {jsx: 'automatic'}
});
