import {panel, row} from '../html.js';
import type {AppState} from '../state.js';

const fmt = (n: number | bigint) => n.toLocaleString('en-GB');

/** What the budget chose, and how close the prediction came. Prediction against reality is the row that matters. */
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
     ${row('predicted marks', chosen ? fmt(chosen.predictedMarks) : '—')}
     ${row('actual marks', fmt(actual))}
     ${row('drift', drift)}
     ${row('m_target (calibrated)', state.mTarget.toFixed(2))}
     ${row('limited by', chosen ? chosen.limitedBy : '—')}`
  );
}
