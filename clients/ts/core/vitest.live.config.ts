import {defaultServerConditions} from 'vite';
import {defineConfig} from 'vitest/config';

// Verbose, so a skipped test prints the reason it was skipped.
export default defineConfig({
  ssr: {resolve: {conditions: ['mosaica-source', ...defaultServerConditions]}},
  test: {environment: 'node', include: ['test/**/*.live.test.ts'], testTimeout: 60_000, reporters: ['verbose']}
});
