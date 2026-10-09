import {defaultServerConditions} from 'vite';
import {defineConfig} from 'vitest/config';

export default defineConfig({
  ssr: {resolve: {conditions: ['mosaica-source', ...defaultServerConditions]}},
  test: {include: ['test/**/*.test.ts']}
});
