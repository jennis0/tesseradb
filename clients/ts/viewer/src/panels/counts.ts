import {panel, row} from '../html.js';
import type {AppState} from '../state.js';

const fmt = (n: bigint | number) => n.toLocaleString('en-GB');

/**
 * Sum the exact masked counts over the tiles deck.gl holds AT THE DEEPEST LOADED DEPTH.
 *
 * The depth filter is load-bearing, not tidiness. `best-available` refinement keeps a parent tile
 * on screen while its children load, and a parent's counts cover the same region as its four
 * children's — summing both double-counts, and the number that would be wrong is precisely the one
 * this panel exists to be right about.
 *
 * Nothing here is derived from the drawn marks. `served` is what was drawn; `visible` is what
 * exists inside the mask. Both are shown, always, because a sample must never read as a set (P2).
 */
/** A failure within this window is treated as describing the current display. */
const RECENT_FAILURE_MS = 10_000;

export function renderCounts(state: AppState): string {
  const tiles = [...state.tiles.values()];
  if (tiles.length === 0) {
    // Zero and unknown are not the same answer, and this is the one place the difference is
    // cheapest to lose: with nothing loaded, "no tiles" reads as an empty region when what
    // actually happened may be that every request was refused.
    const recentlyFailed = state.failures.some(
      (f) => Date.now() - f.at < RECENT_FAILURE_MS
    );
    // Loading is not empty either. At 1e9 a broad principal's shallow tiles take over a second,
    // so a plain "no tiles" reads as "this principal sees nothing" during every pan — which is
    // the same collapse one step along. The MVP does not carry the full four-state treatment; it
    // does carry the three states that are actively misleading when merged.
    if (recentlyFailed) {
      return panel(
        'Counts',
        '<div class="bad">counts unavailable — requests failed, see below. This is not an empty region.</div>'
      );
    }
    return panel(
      'Counts',
      state.inFlight > 0
        ? `<div class="muted">loading ${state.inFlight} tile${state.inFlight === 1 ? '' : 's'}…</div>`
        : '<div class="muted">no tiles loaded</div>'
    );
  }
  const depth = Math.max(...tiles.map((t) => t.z));
  const current = tiles.filter((t) => t.z === depth);

  let visible = 0n;
  let matched = 0n;
  let served = 0n;
  let nonEmpty = 0;
  for (const tile of current) {
    for (const counts of tile.counts) {
      visible += counts.visible;
      matched += counts.matched;
      served += counts.served;
      nonEmpty += 1;
    }
  }

  const complete = served === visible;
  return panel(
    'Counts',
    `${row('served (drawn)', fmt(served))}
     ${row('visible (in mask)', fmt(visible))}
     ${row('matched', fmt(matched))}
     ${row('depth', String(depth))}
     ${row('non-empty tiles', fmt(nonEmpty))}
     <div class="headline">${fmt(served)} of ${fmt(visible)} shown${
       complete ? ' — all of them' : ''
     }</div>`
  );
}
