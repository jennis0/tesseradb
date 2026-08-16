import {esc, panel, row} from '../html.js';
import type {AppState} from '../state.js';

const fmt = (n: number | bigint) => n.toLocaleString('en-GB');

/**
 * Which annotation layer the map draws — the control, in the left column.
 *
 * The list comes from `/v1/meta`, which is gate-filtered, so **this is the whole of what a viewer
 * may know exists**. A layer this principal cannot reach is not greyed out or listed as forbidden;
 * it is absent, by exactly the route a name that was never registered takes. Nothing here offers a
 * way to ask about one.
 */
export function renderLayerControl(state: AppState): string {
  const layers = state.meta?.layers ?? [];
  if (layers.length === 0) {
    return panel(
      'Annotation layers',
      `<div class="muted">this principal reaches no layer here — which is also what a deployment
        with none looks like. <code>scripts/publish-clusters.mjs</code> registers one.</div>`
    );
  }

  const options = [
    `<option value=""${state.artifactLayer === null ? ' selected' : ''}>— none —</option>`,
    ...layers.map(
      (l) =>
        `<option value="${esc(l.name)}"${l.name === state.artifactLayer ? ' selected' : ''}>${esc(
          l.title || l.name
        )}</option>`
    )
  ].join('');

  const chosen = layers.find((l) => l.name === state.artifactLayer);
  return panel(
    'Annotation layers',
    `<div class="ctl">
       <div class="ctl-name${state.artifactLayer ? ' on' : ''}">layer</div>
       <div class="ctl-row"><select id="artifact-layer" class="grow">${options}</select></div>
     </div>
     ${chosen ? row('name', chosen.name) : ''}
     ${chosen ? row('membership', chosen.membership) : ''}
     ${chosen ? row('hierarchy', chosen.hierarchy.kind) : ''}
     <div class="muted">how many artifacts a layer holds is never published: what you reach of one
       is answered artifact by artifact, by the viewport.</div>`
  );
}

/**
 * What the current view is served, and the one number beside each cluster — the readout column.
 *
 * Three things this panel is careful about, each of them a way the picture could be misread:
 *
 * **The count is over the whole cluster, not over the viewport**, so it does not move as the map
 * does. A per-viewport count would let a viewer difference two boxes and recover the members in
 * between. Only *whether* a cluster appears depends on where you are looking.
 *
 * **It is this principal's count, and never the cluster's size.** Two principals get different
 * numbers for the same identifier and neither is the size; a surface calling it "the cluster's
 * size" would be asserting the bug the whole mechanism exists to prevent.
 *
 * **An absent cluster carries no reason.** Below its layer's criterion, in a layer this principal
 * cannot reach, suppressed, never published — one answer, no way to tell them apart, so there is
 * no "hidden" row here and nothing to fill one from.
 */
export function renderArtifacts(state: AppState): string {
  if (!state.artifactLayer) {
    return panel('Clusters in view', '<div class="muted">no layer selected</div>');
  }
  switch (state.artifactStatus) {
    case 'idle':
      return panel('Clusters in view', '<div class="muted">waiting for the first view</div>');
    case 'loading':
    case 'retrying':
      return panel('Clusters in view', '<div class="muted">loading…</div>');
    case 'refused':
      return panel(
        'Clusters in view',
        `<div class="bad">the artifact request was refused${
          state.artifactError ? `: ${esc(state.artifactError.code)}` : ''
        }. This is not an empty view.</div>`
      );
    default:
      break;
  }

  const artifacts = state.artifacts;
  if (artifacts.length === 0) {
    return panel(
      'Clusters in view',
      `<div class="muted">nothing served here — no cluster in this view has a member this principal
        can see, or none clears its layer's existence criterion. The response does not say which,
        and a cluster that was never published looks the same.</div>`
    );
  }

  const sorted = [...artifacts].sort((a, b) => (a.maskedCount < b.maskedCount ? 1 : -1));
  const rows = sorted
    .slice(0, 12)
    .map((a) => row(a.stableKey ?? `#${a.tesseraId}`, fmt(a.maskedCount)))
    .join('');

  return panel(
    'Clusters in view',
    `<div class="headline">${fmt(artifacts.length)} served</div>
     ${rows}
     ${sorted.length > 12 ? `<div class="muted">…and ${fmt(sorted.length - 12)} more</div>` : ''}
     <div class="muted">members visible to this principal, over the whole cluster — not over the
       viewport, so it holds steady as you pan. Never the cluster's size.</div>`
  );
}

/** The opened cluster: the same count, from the same predicate, by identifier. */
export function renderArtifactDetail(state: AppState): string {
  if (state.artifactDetailError) {
    return panel(
      'Cluster',
      `<div class="bad">${esc(state.artifactDetailError.code)}: ${esc(
        state.artifactDetailError.detail
      )}</div>
       <div class="muted">one refusal covers every withheld case — an identifier naming nothing, one
         naming a document, one whose layer this principal cannot reach, one suppressed, one below
         its criterion. There is nothing here that tells them apart.</div>`
    );
  }
  const opened = state.selectedArtifact;
  if (!opened) return panel('Cluster', '<div class="muted">click a cluster</div>');
  return panel(
    'Cluster',
    `${row('layer', opened.layer)}
     ${row('stable key', opened.stableKey ?? '— none supplied —')}
     ${row('tessera_id', opened.id.toString())}
     <div class="headline">${fmt(opened.maskedCount)} members you can see</div>
     <div class="muted">the number the map already carried, from the same predicate. There is no
       membership and no declared size behind it.</div>`
  );
}
