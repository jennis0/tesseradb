import {defineConfig} from 'vitest/config';

/**
 * The tests that need a real browser's cascade: they build the single-file bundle and drive it in
 * headless Chromium through Playwright (`npm run test:browser`). happy-dom does not implement
 * custom-property inheritance across shadow roots, `::part` or `exportparts`.
 */
export default defineConfig({
  test: {environment: 'node', include: ['test/browser/**/*.browser.ts'], testTimeout: 60_000, hookTimeout: 120_000}
});
