import {defaultClientConditions} from 'vite';
import {defineConfig} from 'vitest/config';

export default defineConfig({
  resolve: {conditions: ['tessera-source', ...defaultClientConditions]},
  test: {environment: 'happy-dom', include: ['test/**/*.test.ts']}
});
