import {esc, panel, row} from '../html.js';
import type {AppState} from '../state.js';
import type {Dataset, Preset} from '../config.js';

/**
 * What is being looked at: which bundle, as whom, and how many marks.
 *
 * A dataset is a whole server: `tessera serve` serves one bundle, so switching re-authorises
 * against another server and drops every held band. The picker appears only when more than one is
 * running. `prose indexed` shows which text columns a bundle has, since a missing one otherwise
 * looks like a broken control.
 *
 * A principal's `visible at build` is what `scripts/measure-principals.mjs` measured over the full
 * extent: the whole visible set, not the viewport's `visible`. A filter does not change it.
 *
 * Depth is chosen from the mark budget, not the zoom, so marks on screen stay roughly constant.
 * Calibration only goes deeper, since a shallower request would serve a subset of what is drawn.
 */
export function renderSource(state: AppState, datasets: Dataset[], presets: Preset[]): string {
  const current = datasets.find((d) => d.id === state.datasetId);

  const dataset =
    datasets.length > 1
      ? `<select id="dataset">${datasets
          .map(
            (d) =>
              `<option value="${esc(d.id)}"${d.id === state.datasetId ? ' selected' : ''}>${esc(
                d.label
              )}</option>`
          )
          .join('')}</select>`
      : row('bundle', current?.label ?? '—');

  const principal =
    presets.length > 0
      ? `<select id="principal">${presets
          .map(
            (p, i) =>
              `<option value="${i}"${p.label === state.termsLabel ? ' selected' : ''}>${esc(
                p.label
              )}</option>`
          )
          .join('')}</select>`
      : `<div class="muted">no measured presets; run_demo.sh writes them per bundle</div>`;

  const active = presets.find((p) => p.label === state.termsLabel);
  const fmt = (n: number) => n.toLocaleString('en-GB');

  return panel(
    'Source',
    `${dataset}
     ${principal}
     <input id="budget" type="range" min="1000" max="2000000" step="1000" value="${state.budget}" />
     ${row('mark budget', fmt(state.budget))}
     ${row('visible at build', active ? fmt(active.visible) : '—')}
     ${row('prose indexed', current?.prose.length ? current.prose.join(', ') : 'none')}
     ${state.switching ? '<div class="muted">switching: establishing a session…</div>' : ''}`
  );
}
