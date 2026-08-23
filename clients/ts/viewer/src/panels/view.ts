import {panel, row} from '../html.js';
import type {AppState} from '../state.js';

const fmt = (n: number | bigint) => n.toLocaleString('en-GB');

/**
 * Counts for the current view — and the three numbers whose *relationship* is the point.
 *
 * `visible` is the mask over the drawn region: what this principal may see there, and a filter must
 * never move it. `matched` is what the filter admits from inside that. `served` is what was drawn,
 * which is a sample of `matched` chosen for the mark budget. Read in that order, the panel is the
 * one place a viewer can see that filtering narrows the *answer* without touching the *grant*; a
 * surface showing any one of them alone lets a sample read as a set.
 *
 * The figures cover the drawn region, which reaches well beyond the viewport so that panning inside
 * it costs neither a request nor a redraw — so they are exact masked figures for that region and not
 * for the visible rectangle.
 *
 * **Counts come only from exact tiles**, and provisional marks are reported as a count of marks
 * rather than folded into any figure: a tile drawn from an ancestor or from held descendants shows a
 * superset of its served set, and reading a superset as density is what `delta-serving.md` §7
 * forbids.
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
    case 'loading': {
      if (!state.sessionWarm) {
        const waited = state.depthChoice
          ? Math.round((Date.now() - state.depthChoice.requestedAt) / 1000)
          : 0;
        // The visible set materialises inside the session's first request, and at a large corpus a
        // broad principal's union takes seconds — a different wait from every later "loading", so
        // it says what is happening rather than reading as a hung fetch.
        return panel(
          'Counts',
          `<div class="muted">establishing this principal's visible set — the first request of a
           session materialises the mask, which can take seconds on a large corpus
           (${waited}s)…</div>`
        );
      }
      return panel('Counts', '<div class="muted">loading…</div>');
    }
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

  const assembled = state.assembled;
  if (!assembled) return panel('Counts', '<div class="muted">no data</div>');

  // **Counts come only from exact tiles.** A tile drawn from an ancestor or from held descendants
  // shows a superset of its served set, and reading a superset as density is the failure
  // `caching.md` §6 guards against — so those tiles contribute marks to the picture and nothing at
  // all to the numbers.
  let visible = 0n;
  let matched = 0n;
  let served = 0n;
  let exactTiles = 0;
  for (const tile of assembled.tiles) {
    if (!tile.counts) continue;
    visible += tile.counts.visible;
    matched += tile.counts.matched;
    served += BigInt(tile.counts.served);
    exactTiles++;
  }

  const provisional =
    assembled.provisional > 0
      ? `${row('provisional (uncounted)', fmt(BigInt(assembled.provisional)))}`
      : '';

  return panel(
    'Counts',
    `${row('visible (in mask)', fmt(visible))}
     ${row('matched (after filters)', fmt(matched))}
     ${row('served (drawn)', fmt(served))}
     ${provisional}
     <div class="headline">${fmt(served)} of ${fmt(visible)} shown</div>
     <div class="muted">exact, over the drawn region — wider than the viewport</div>`
  );
}

/** What the budget chose, and how close the prediction came. Prediction against reality is the row that matters. */
export function renderDepth(state: AppState): string {
  const chosen = state.depthChoice;
  // Against the prediction, only exact tiles are comparable — they are what the budget asked for.
  const actual = state.assembled?.exactDrawn ?? 0;
  const drift =
    chosen && chosen.predictedMarks > 0
      ? `${(((actual - chosen.predictedMarks) / chosen.predictedMarks) * 100).toFixed(0)}%`
      : '—';

  return panel(
    'Depth chosen',
    `${row('depth', chosen ? String(chosen.depth) : '—')}
     ${row('tiles requested', chosen ? fmt(chosen.tiles) : '—')}
     ${row('predicted marks', chosen ? fmt(chosen.predictedMarks) : '—')}
     ${row('actual marks', fmt(actual))}
     ${row('drift', drift)}
     ${row('m_target (calibrated)', state.mTarget.toFixed(2))}
     ${row('limited by', chosen ? chosen.limitedBy : '—')}`
  );
}
