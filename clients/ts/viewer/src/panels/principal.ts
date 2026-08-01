import {esc, panel, row} from '../html.js';
import type {AppState} from '../state.js';

export type Preset = {label: string; terms: string[]; visible: number};

/**
 * The principal switcher.
 *
 * `visible at build` is the figure `scripts/measure-principals.mjs` measured with a zoom-0
 * full-extent call — the principal's whole visible set, independent of where the map is looking.
 * The counts panel's `visible` is the viewport's. They are different quantities and the labels say
 * so, because a viewer who reads one as the other has misunderstood the only thing this instrument
 * is for.
 */
export function renderPrincipal(state: AppState, presets: Preset[]): string {
  const options = presets
    .map(
      (p, i) =>
        `<option value="${i}"${p.label === state.termsLabel ? ' selected' : ''}>${esc(
          p.label
        )}</option>`
    )
    .join('');
  const active = presets.find((p) => p.label === state.termsLabel);
  return panel(
    'Principal',
    `<select id="principal">${options}</select>
     ${row('terms granted', String(state.terms.length))}
     ${row('visible at build', active ? active.visible.toLocaleString('en-GB') : '—')}`
  );
}
