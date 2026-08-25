import {esc, panel} from '../html.js';
import type {AppState} from '../state.js';

/**
 * A failed tile is not an empty tile.
 *
 * An empty viewport and a failed viewport are semantic opposites — zero versus unknown — and
 * rendering a failure as blank space converts a fail-closed server into a fail-misleading picture.
 * The MVP does not carry the full four-state treatment; this panel is the part of it that must not
 * wait, because it is the part that turns a server's honesty into a lie.
 */
export function renderErrors(state: AppState): string {
  if (state.failures.length === 0) return '';
  const rows = state.failures
    .slice(-6)
    .reverse()
    .map((f) => `<div class="bad">${new Date(f.at).toISOString().slice(11, 19)} — ${esc(f.code)}: ${esc(f.detail)}</div>`)
    .join('');
  return panel(`Failures (${state.failures.length})`, rows);
}
