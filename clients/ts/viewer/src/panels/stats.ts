import {panel, row} from '../html.js';
import type {AppState} from '../state.js';

/**
 * `x-tessera-stage-ns` is a positional CSV with no names — the field order is contract with
 * `scripts/bench_*.py` and `tessera-bench`, and it is **append-only**. Mirrored here; if the
 * server appends a field, append here.
 */
const STAGE_FIELDS = [
  'generation_resolve_ns',
  'stamp_compare_ns',
  'slice_lookup_ns',
  'row_projection_ns',
  'compose_ns',
  'tiles_for_bbox_ns',
  'tile_ranges_ns',
  'count_ns',
  'select_ns',
  'gather_ns',
  'arrow_serialise_ns',
  'total_ns',
  'tiles_resolved',
  'tiles_nonempty',
  'sigma_visible',
  'rows_in_ranges',
  'select_rows_visited',
  'points_gathered',
  'row_projection_built',
  'theta_anchor_ns',
  'underlay_ns',
  'underlay_cells_evaluated'
] as const;

const INTERESTING = ['count_ns', 'select_ns', 'gather_ns', 'arrow_serialise_ns', 'total_ns'];

export function renderStats(state: AppState): string {
  const t = state.lastTimings;
  const drawn = state.result?.ids.length ?? 0;

  const stage = t?.stageNs
    ? INTERESTING.map((name) => {
        const index = STAGE_FIELDS.indexOf(name as (typeof STAGE_FIELDS)[number]);
        const ns = t.stageNs![index] ?? 0;
        return row(name.replace(/_ns$/, ''), `${(ns / 1e6).toFixed(2)} ms`);
      }).join('')
    : `<div class="muted">stage timings absent — the server was built without the
        <code>bench-timing</code> feature, or <code>[serve] stage_timing</code> is false. Not an
        error.</div>`;

  const l = state.latency;
  const lag = l
    ? `${row('— waited (debounce)', `${l.waited} ms`)}
       ${row('— fetch + decode', `${l.fetch} ms`)}
       ${row('— of which server', `${l.server} ms`)}
       <div class="headline">pan to paint: ${l.total} ms</div>`
    : '';

  return panel(
    'Last request',
    `${lag}
     ${row('server', t ? `${(t.serverUs / 1000).toFixed(1)} ms` : '—')}
     ${row('admission', t ? `${(t.admissionUs / 1000).toFixed(1)} ms` : '—')}
     ${row('bytes', state.lastBytes.toLocaleString('en-GB'))}
     ${row('in flight', String(state.inFlight))}
     ${row('tiles in view', String(state.result?.tiles.length ?? 0))}
     ${row('marks drawn', drawn.toLocaleString('en-GB'))}
     ${stage}`
  );
}

/**
 * `k` and the underlay controls were removed with the tile-addressed layer.
 *
 * `k` is a per-tile cap that θ almost never reaches (measured: inert at every depth on every
 * fixture), and the quantity a user actually wants to set — marks on screen — is now the budget,
 * which `panels/view.ts` owns. The underlay moves to a cached per-session density pyramid under a
 * separate design (owner ruling, 2026-08-01), so a per-request offset slider would model something
 * the client no longer does.
 */
