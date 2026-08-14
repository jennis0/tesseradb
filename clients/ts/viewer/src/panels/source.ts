import {esc, panel, row} from '../html.js';
import type {AppState} from '../state.js';
import type {Dataset, Preset} from '../config.js';

/**
 * What is being looked at: which bundle, as whom, and how many marks.
 *
 * **One panel for three controls**, because they are the three answers a session is configured with
 * and each carried a heading and a paragraph of its own — which pushed the filters, the thing this
 * demo now exists to show, below the fold. The explanations moved here, to the module doc, which is
 * where this repository keeps a design argument; a control column is for controls.
 *
 * ## Dataset
 *
 * **A dataset is a whole server, not a parameter.** `[bundle] path` is one path per `tessera serve`
 * process, so two bundles are two processes on two port pairs, and switching means re-authorising
 * against a different session plane and dropping every held band — a new identity space, a new term
 * dictionary and a new schema. The picker appears only when more than one is running: the control
 * exists because there is a choice, not because the code has one.
 *
 * `prose indexed` is worth its row because it is the reason to switch. The small bundle carries
 * abstracts and the large one does not, and a reader who does not know that reads the missing
 * abstract box as a broken control rather than as an absent column.
 *
 * ## Principal
 *
 * `visible at build` is what `scripts/measure-principals.mjs` measured with a zoom-0, full-extent
 * call: the principal's whole visible set, independent of where the map is looking. The counts
 * panel's `visible` is the viewport's. They are different quantities, and a viewer who reads one as
 * the other has misunderstood the only thing this instrument is for.
 *
 * **A filter does not move `visible at build`, and must not.** The figure is the mask's cardinality;
 * a filter narrows what is *served* from inside it. Showing a filtered count here would let the
 * mask's size be read off a filter, which is the composition the counts panel keeps separate.
 *
 * ## Mark budget
 *
 * Depth is chosen for the budget rather than from the zoom, so marks on screen stay roughly constant
 * as you zoom. Calibration only ever goes deeper: a shallower request would serve a subset of what is
 * already drawn. What the budget then *did* — the depth it chose, the drift of reality against
 * prediction — is a readout, and lives in `renderDepth` on the other side.
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
      : `<div class="muted">no measured presets — run_demo.sh writes them per bundle</div>`;

  const active = presets.find((p) => p.label === state.termsLabel);
  const fmt = (n: number) => n.toLocaleString('en-GB');

  return panel(
    'Source',
    `${dataset}
     ${principal}
     <input id="budget" type="range" min="1000" max="500000" step="1000" value="${state.budget}" />
     ${row('mark budget', fmt(state.budget))}
     ${row('visible at build', active ? fmt(active.visible) : '—')}
     ${row('prose indexed', current?.prose.length ? current.prose.join(', ') : 'none')}
     ${state.switching ? '<div class="muted">switching — establishing a session…</div>' : ''}`
  );
}
