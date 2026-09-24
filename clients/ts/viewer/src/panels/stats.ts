import {panel, row} from '../html.js';
import type {AppState} from '../state.js';

/**
 * The fields of `x-tessera-stage-ns`, a positional CSV. The order is shared with
 * `scripts/bench_*.py` and `tessera-bench` and only grows at the end.
 */
const STAGE_FIELDS = [
  'generation_resolve_ns',
  'stamp_compare_ns',
  'view_lookup_ns',
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
  'underlay_cells_evaluated',
  'shape_guard_fired',
  'theta_occupancy_ns'
] as const;

const INTERESTING = ['count_ns', 'select_ns', 'gather_ns', 'arrow_serialise_ns', 'total_ns'];

/**
 * Whether the instrument drawer is open. The panels are rebuilt from `innerHTML` on every store
 * change, so a `<details>` element cannot keep its own open flag.
 */
let drawerOpen = false;

/** Called by the click handler in `main.ts`. */
export function toggleStatsDrawer(open: boolean): void {
  drawerOpen = open;
}

/**
 * What the last view cost, and what the replica saved: six rows, with the stage breakdown and the
 * replica's residency in a drawer. `residency` comes from the slab, not the store, so a redraw
 * that changes nothing is not a state change.
 */
export function renderStats(state: AppState, residency: {drawn: number; departed: number}): string {
  const t = state.lastTimings;
  const provisional = state.frame?.provisional ?? 0;
  const drawn = residency.drawn + provisional;
  const l = state.latency;

  const headline = l
    ? `<div class="headline">pan to paint: ${l.total} ms</div>`
    : '<div class="muted">no gesture timed yet</div>';

  const cache = state.lastPlan
    ? `${state.lastPlan.omitted} of ${state.lastPlan.omitted + state.lastPlan.fetched}`
    : '—';

  const core = `${row('server', t ? `${(t.serverUs / 1000).toFixed(1)} ms` : '—')}
     ${row('fetch + decode', l ? `${l.fetch} ms` : '—')}
     ${row('bytes', state.lastBytes.toLocaleString('en-GB'))}
     ${row('marks drawn', drawn.toLocaleString('en-GB'))}
     ${row('tiles from cache', cache)}
     ${row('replica held', `${((state.replicaBytes ?? 0) / 1e6).toFixed(1)} MB`)}`;

  return panel('Last request', `${headline}${core}${drawer(state, residency, provisional)}`);
}

/** The drawer's rows. */
function drawer(
  state: AppState,
  residency: {drawn: number; departed: number},
  provisional: number
): string {
  const t = state.lastTimings;
  const stage = t?.stageNs
    ? INTERESTING.map((name) => {
        const index = STAGE_FIELDS.indexOf(name as (typeof STAGE_FIELDS)[number]);
        const ns = t.stageNs![index] ?? 0;
        return row(name.replace(/_ns$/, ''), `${(ns / 1e6).toFixed(2)} ms`);
      }).join('')
    : `<div class="muted">stage timings absent: the server was built without the
        <code>bench-timing</code> feature, or <code>[serve] stage_timing</code> is false. Not an
        error.</div>`;

  const l = state.latency;
  return `<details id="stats-drawer"${drawerOpen ? ' open' : ''}>
      <summary>replica and stage timings</summary>
      ${row('waited (debounce)', l ? `${l.waited} ms` : '—')}
      ${row('admission', t ? `${(t.admissionUs / 1000).toFixed(1)} ms` : '—')}
      ${row('in flight', String(state.inFlight))}
      ${row('tiles in view', String(state.frame?.tiles.length ?? 0))}
      ${row('of which provisional', provisional.toLocaleString('en-GB'))}
      ${row('retained off-view', residency.departed.toLocaleString('en-GB'))}
      ${row('replica points', (state.replicaPoints ?? 0).toLocaleString('en-GB'))}
      ${row('replica bands', (state.replicaBands ?? 0).toLocaleString('en-GB'))}
      ${row('bytes/point', state.replicaPoints ? `${Math.round((state.replicaBytes ?? 0) / state.replicaPoints)} B` : '—')}
      ${row('prefetched ahead', String(state.prefetched ?? 0))}
      ${stage}
    </details>`;
}
