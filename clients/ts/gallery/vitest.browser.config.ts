import {defineConfig} from 'vitest/config';

/** The smoke test: the gallery served by Vite and loaded in headless Chromium through Playwright. */
export default defineConfig({
  test: {environment: 'node', include: ['test/**/*.browser.ts'], testTimeout: 120_000, hookTimeout: 120_000}
});
