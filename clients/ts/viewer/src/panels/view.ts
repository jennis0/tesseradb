import {panel, row} from '../html.js';
import type {AppState} from '../state.js';

const fmt = (n: number | bigint) => n.toLocaleString('en-GB');

/**
 * Counts for the current view.
 *
 * One request per view means there is no per-tile depth mixing to guard against any more — every
 * tile in `result` is at the same depth by construction, which removes the double-counting hazard
 * the tile-addressed version had to filter for.
 *
 * The four display states stay distinct (client-interaction §9). Only `shown` may display counts:
 * a number rendered beside a stale or refused view is worse than no number.
 */
export function renderCounts(state: AppState): string {
  switch (state.status) {
    case 'idle':
      return panel('Counts', '<div class="muted">waiting for the first view</div>');
    case 'loading':
      return panel('Counts', '<div class="muted">loading…</div>');
    case 'retrying':
      return panel(
        'Counts',
        '<div class="bad">the server shed this request (backpressure) — retrying</div>'
      );
    case 'refused':
      return panel(
        'Counts',
        `<div class="bad">counts unavailable — the request was refused${
          state.lastError ? `: ${state.lastError.code}` : ''
        }. This is not an empty region.</div>`
      );
    case 'empty':
      return panel(
        'Counts',
        '<div class="muted">this principal sees nothing in this view — an actual zero, not a failure</div>'
      );
    case 'shown':
      break;
  }

  const result = state.result;
  if (!result) return panel('Counts', '<div class="muted">no data</div>');

  let visible = 0n;
  let matched = 0n;
  let served = 0n;
  for (const t of result.tiles) {
    visible += t.visible;
    matched += t.matched;
    served += t.served;
  }

  return panel(
    'Counts',
    `${row('served (drawn)', fmt(served))}
     ${row('visible (in mask)', fmt(visible))}
     ${row('matched', fmt(matched))}
     ${row('non-empty tiles', fmt(result.tiles.length))}
     <div class="headline">${fmt(served)} of ${fmt(visible)} shown</div>`
  );
}

/** The budget readout — prediction against reality is the row that matters. */
export function renderBudget(state: AppState): string {
  const view = state.view;
  const actual = state.result?.ids.length ?? 0;
  const drift =
    view && view.predictedMarks > 0
      ? `${(((actual - view.predictedMarks) / view.predictedMarks) * 100).toFixed(0)}%`
      : '—';

  return panel(
    'Mark budget',
    `<input id="budget" type="range" min="1000" max="500000" step="1000" value="${state.budget}" />
     ${row('budget', fmt(state.budget))}
     ${row('depth chosen', view ? String(view.depth) : '—')}
     ${row('tiles requested', view ? fmt(view.tiles) : '—')}
     ${row('predicted marks', view ? fmt(view.predictedMarks) : '—')}
     ${row('actual marks', fmt(actual))}
     ${row('drift', drift)}
     ${row('m_target (calibrated)', state.mTarget.toFixed(2))}
     ${row('limited by', view ? view.limitedBy : '—')}
     <div class="muted">depth is chosen for the budget, not from the zoom — marks on screen stay
       roughly constant as you zoom. Calibration only ever goes deeper: a shallower request would
       serve a subset of what is already drawn.</div>`
  );
}
