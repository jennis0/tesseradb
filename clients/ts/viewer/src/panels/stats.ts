import {panel, row} from '../html.js';
import type {AppState} from '../state.js';

/**
 * `x-tessera-stage-ns` is a positional CSV with no names — the field order is contract with
 * `scripts/bench_*.py` and `tessera-bench`, and it is **append-only**. Mirrored here; if the
 * server appends a field, append here.
 */
const STAGE_FIELDS = [
  'generation_resolve_ns',
  'pin_resolve_ns',
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
  const drawn = [...state.tiles.values()].reduce((a, tile) => a + tile.pointCount, 0);

  const stage = t?.stageNs
    ? INTERESTING.map((name) => {
        const index = STAGE_FIELDS.indexOf(name as (typeof STAGE_FIELDS)[number]);
        const ns = t.stageNs![index] ?? 0;
        return row(name.replace(/_ns$/, ''), `${(ns / 1e6).toFixed(2)} ms`);
      }).join('')
    : `<div class="muted">stage timings absent — the server was built without the
        <code>bench-timing</code> feature, or <code>[serve] stage_timing</code> is false. Not an
        error.</div>`;

  return panel(
    'Last request',
    `${row('server', t ? `${(t.serverUs / 1000).toFixed(1)} ms` : '—')}
     ${row('admission', t ? `${(t.admissionUs / 1000).toFixed(1)} ms` : '—')}
     ${row('bytes', state.lastBytes.toLocaleString('en-GB'))}
     ${row('in flight', String(state.inFlight))}
     ${row('tiles held', String(state.tiles.size))}
     ${row('marks drawn', drawn.toLocaleString('en-GB'))}
     ${stage}`
  );
}

/**
 * The `k` control.
 *
 * Undefined `k` means the request omits it entirely, so the deployment's own ceiling applies —
 * contracts §3.2, and the reason a caller who never touches this slider cannot decrease `k`. Once
 * touched, it can: the MVP has no replica store to own `k`, so P6's non-decreasing obligation is
 * not held here. The panel says so rather than leaving it to be discovered.
 */
export function renderK(state: AppState): string {
  const max = state.meta?.selection.maxK ?? 1000;
  const fallback = state.meta?.selection.kMaxMarks ?? max;
  const value = state.k ?? fallback;

  // Which of §7.2's clauses is actually deciding? The cap `k` binds only when the threshold θ
  // would admit MORE than k; below that, moving this slider changes nothing and the map is right
  // to ignore it. Saying so turns an inert-looking control into a reading of the sampler, which is
  // what an instrument is for — and it is measured from the served counts rather than asserted.
  let peak = 0;
  for (const tile of state.tiles.values()) {
    for (const counts of tile.counts) peak = Math.max(peak, Number(counts.served));
  }
  const binding =
    state.tiles.size === 0
      ? '<div class="muted">no tiles loaded</div>'
      : peak >= value
        ? `<div class="headline">k is binding — the busiest tile served ${peak.toLocaleString(
            'en-GB'
          )}, at the cap</div>`
        : `<div class="headline muted">k is not binding — the busiest tile served
            ${peak.toLocaleString('en-GB')}, below the cap, so θ (the threshold clause) is what
            decides here. Raise <code>serve.theta_target_marks</code> to make k bite.</div>`;

  return panel(
    'k — marks per tile',
    `<input id="k" type="range" min="1" max="${max}" value="${value}" />
     ${row('k', state.k === undefined ? `${value} (deployment default)` : String(state.k))}
     ${row('k_max_marks', String(fallback))}
     ${row('max_k', String(max))}
     ${row('θ target marks', String(state.meta?.selection.thetaTargetMarks ?? '—'))}
     ${binding}
     <div class="muted">the MVP has no replica store to hold k non-decreasing, so this slider can
       lower it — a deliberate deviation from P6</div>`
  );
}

/**
 * The density-underlay control. `underlay_offset` is rejected rather than clamped by the server on
 * all three of its bounds, so the maximum here comes from `/v1/meta` rather than a literal.
 */
export function renderUnderlay(state: AppState): string {
  const max = state.meta?.selection.maxUnderlayOffset ?? 0;
  return panel(
    'Density underlay',
    `<input id="underlay" type="range" min="0" max="${max}" value="${state.underlayOffset}" />
     ${row('offset', state.underlayOffset === 0 ? 'off' : `+${state.underlayOffset}`)}
     ${row('sub-cells per tile', (4 ** state.underlayOffset).toLocaleString('en-GB'))}`
  );
}
