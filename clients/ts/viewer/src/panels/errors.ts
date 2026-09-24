import {esc, panel} from '../html.js';
import type {AppState} from '../state.js';

/**
 * The refusals the store reported, so a failed viewport (unknown) is not read as an empty one
 * (zero).
 */
export function renderErrors(state: AppState): string {
  if (state.failures.length === 0) return '';
  const rows = state.failures
    .slice(-6)
    .reverse()
    .map((f) => `<div class="bad">${new Date(f.at).toISOString().slice(11, 19)} ${esc(f.code)}: ${esc(f.detail)}</div>`)
    .join('');
  return panel(`Failures (${state.failures.length})`, rows);
}
