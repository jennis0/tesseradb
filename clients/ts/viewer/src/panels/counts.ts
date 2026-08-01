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
export function renderCounts(state: AppState): string {
  const tiles = [...state.tiles.values()];
  if (tiles.length === 0) {
    return panel('Counts', '<div class="muted">no tiles loaded</div>');
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
