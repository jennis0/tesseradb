import {panel, row} from '../html.js';
import type {AppState} from '../state.js';

const fmt = (n: number | bigint) => n.toLocaleString('en-GB');

/**
 * What the budget chose, and how close the prediction came. Prediction against reality is the row
 * that matters, and *from* says which model made it: `counts` is arithmetic over the masked counts
 * the responses carried and should read near zero drift, `average` the `m_target` fallback for a
 * view no counts cover, `bound` the counts moved to another depth.
 */
export function renderDepth(state: AppState): string {
  const chosen = state.depthChoice;
  // Against the prediction, only exact tiles are comparable — they are what the budget asked for.
  const actual = state.frame?.exactDrawn ?? 0;
  const drift =
    chosen && chosen.predictedMarks > 0
      ? `${(((actual - chosen.predictedMarks) / chosen.predictedMarks) * 100).toFixed(0)}%`
      : '—';

  return panel(
    'Depth chosen',
    `${row('depth', chosen ? String(chosen.depth) : '—')}
     ${row('tiles requested', chosen ? fmt(chosen.tiles) : '—')}
     ${row('predicted marks', chosen ? fmt(Math.round(chosen.predictedMarks)) : '—')}
     ${row('from', chosen ? chosen.source : '—')}
     ${row('actual marks', fmt(actual))}
     ${row('drift', drift)}
     ${row('m_target (calibrated)', state.mTarget.toFixed(2))}
     ${row('limited by', chosen ? chosen.limitedBy : '—')}`
  );
}
