import {defineConfig} from 'vitest/config';

/**
 * `@lit/react`'s `node` export condition is the SSR build, which sets no properties on the element
 * and leaves them to Lit's hydration. A page runs the `browser` build, so the tests resolve that.
 */
export default defineConfig({
  resolve: {conditions: ['browser']},
  test: {environment: 'happy-dom', include: ['test/**/*.test.ts', 'test/**/*.test.tsx']},
  esbuild: {jsx: 'automatic'}
});
