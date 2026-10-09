import {defaultClientConditions} from 'vite';
import {defineConfig} from 'vitest/config';

export default defineConfig({
  resolve: {conditions: ['mosaica-source', ...defaultClientConditions]},
  test: {environment: 'happy-dom', include: ['test/**/*.test.ts']}
});
