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
 * Whether the instrument drawer is open.
 *
 * Module state rather than store state on purpose: the panels are rebuilt from `innerHTML` on every
 * store change, so a `<details>` element's own open flag is destroyed several times a second and
 * cannot hold this. It is presentation and nothing subscribes to it, so putting it in the store
 * would make every reader of that type wonder what depends on it.
 */
let drawerOpen = false;

/** Called by the click handler in `main.ts` — see {@link drawerOpen}. */
export function toggleStatsDrawer(open: boolean): void {
  drawerOpen = open;
}

/**
 * What the last view cost, and what the replica saved.
 *
 * **Six rows above the fold and the instrument below it.** The panel used to carry twenty-two, which
 * is the right number for a measurement session and the wrong one for reading at a glance — the
 * figure that matters (pan to paint) sat below rows nobody consults twice. The drawer holds the ones
 * that answer a specific question when you have one: the stage breakdown, and the replica's own
 * residency, which `run_demo.sh` documents as the way to watch look-ahead work.
 *
 * `residency` comes from the slab rather than from the store: it is GPU-facing storage that outlives
 * every frame, and routing it through state would make a redraw look like a state change on exactly
 * the frames whose whole point is that nothing changed.
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

/** The rows that answer a question you already have, rather than one you might. */
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
    : `<div class="muted">stage timings absent — the server was built without the
        <code>bench-timing</code> feature, or <code>[serve] stage_timing</code> is false. Not an
        error.</div>`;

  const l = state.latency;
  return `<details id="stats-drawer"${drawerOpen ? ' open' : ''}>
      <summary>replica and stage timings</summary>
      ${row('waited (debounce)', l ? `${l.waited} ms` : '—')}
      ${row('admission', t ? `${(t.admissionUs / 1000).toFixed(1)} ms` : '—')}
      ${row('in flight', String(state.inFlight))}
      ${row('tiles in view', String(state.frame?.tiles.length ?? 0))}
      ${row('— of which provisional', provisional.toLocaleString('en-GB'))}
      ${row('— retained off-view', residency.departed.toLocaleString('en-GB'))}
      ${row('replica points', (state.replicaPoints ?? 0).toLocaleString('en-GB'))}
      ${row('replica bands', (state.replicaBands ?? 0).toLocaleString('en-GB'))}
      ${row('bytes/point', state.replicaPoints ? `${Math.round((state.replicaBytes ?? 0) / state.replicaPoints)} B` : '—')}
      ${row('prefetched ahead', String(state.prefetched ?? 0))}
      ${stage}
    </details>`;
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
