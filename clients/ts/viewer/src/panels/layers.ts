import type {Artifact} from '@tessera/client';
import {servedLineage, type ServedLineage} from '../artifacts.js';
import {esc, panel, row} from '../html.js';
import type {AppState} from '../state.js';

const fmt = (n: number | bigint) => n.toLocaleString('en-GB');

/**
 * What to call an artifact on screen: its supplied text where the layer publishes any, and its
 * key otherwise.
 *
 * **The text is not decoration and it is not the key by another name.** Where an artifact carries
 * several ranked descriptions, what arrives is the one *this* principal qualifies for under the
 * containment test — so two viewers can be looking at the same `tesseraId` and correctly reading
 * different words against it. A panel that showed the key instead would hide the one part of the
 * annotation that moves with the mask.
 *
 * The first value, because content is positional to the layer's `suppliedContent` kinds and a
 * label layer's first kind is its text. A layer publishing something else first — a polygon, say —
 * has no name to show here and falls back to the key.
 */
function describe(a: Artifact): string {
  const text = a.content[0];
  if (text !== undefined && text.length > 0) return text;
  return a.key ?? `#${a.tesseraId}`;
}

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

  const lineage = servedLineage(artifacts);
  const listed = flatten(lineage);
  const rows = listed
    .slice(0, ROWS)
    .map(({artifact, depth}) => nested(describe(artifact), fmt(artifact.maskedCount), depth))
    .join('');

  return panel(
    'Clusters in view',
    `<div class="headline">${fmt(artifacts.length)} served${
      lineage.linked ? `, ${fmt(lineage.roots.length)} of them at the top` : ''
    }</div>
     ${rows}
     ${listed.length > ROWS ? `<div class="muted">…and ${fmt(listed.length - ROWS)} more</div>` : ''}
     ${
       lineage.linked
         ? `<div class="muted">indented under what contains it. A cluster shown flush left is one
             whose parent this principal was not served — which is also what having no parent looks
             like, and the response does not say which.</div>`
         : ''
     }
     <div class="muted">members visible to this principal, over the whole cluster — not over the
       viewport, so it holds steady as you pan. Never the cluster's size.</div>`
  );
}

/** How many rows the panel will show before it stops and says how many it did not. */
const ROWS = 14;

/**
 * The served tree as a list, parents immediately above their own children, largest count first at
 * every level.
 *
 * **Depth-first rather than sorted flat, and the difference is the whole point of the panel.** A
 * global sort by count puts a small child pages away from the parent it sits inside, which is the
 * reading the flat list already gave. Here a row's position says what contains it.
 *
 * Ordering within a level is still by count, so the truncation at {@link ROWS} drops the smallest
 * branches rather than an arbitrary tail.
 */
function flatten(lineage: ServedLineage): {artifact: Artifact; depth: number}[] {
  const out: {artifact: Artifact; depth: number}[] = [];
  const bigger = (a: Artifact, b: Artifact) => (a.maskedCount < b.maskedCount ? 1 : -1);
  // Iterative, and with a visited set, for the reason `subtreeOf` gives: this walks data from
  // outside the program, and a malformed response must not take the panel with it.
  const seen = new Set<bigint>();
  // Reversed on the way in, because the stack pops from the end: pushed largest-first, the panel
  // would list every level smallest-first.
  const push = (into: {artifact: Artifact; depth: number}[], of: Artifact[], depth: number) => {
    for (const artifact of [...of].sort(bigger).reverse()) into.push({artifact, depth});
  };
  const stack: {artifact: Artifact; depth: number}[] = [];
  push(stack, lineage.roots, 0);
  while (stack.length > 0) {
    const node = stack.pop()!;
    if (seen.has(node.artifact.tesseraId)) continue;
    seen.add(node.artifact.tesseraId);
    out.push(node);
    push(stack, lineage.childrenOf.get(node.artifact.tesseraId) ?? [], node.depth + 1);
  }
  return out;
}

/** A label/value row indented to its depth in the served tree. */
function nested(label: string, value: string, depth: number): string {
  return `<div class="row"><span class="muted nest" style="--depth: ${depth}">${esc(
    label
  )}</span><span class="v">${esc(value)}</span></div>`;
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
  // **Its place in the tree comes from the served set, not from the drill-down.** Opening an
  // artifact by identifier answers about that artifact alone; the parent link is a property of a
  // *response*, since a parent is named only where it was served alongside. So this reads the
  // artifacts the current view holds — and where the opened cluster is not among them, because the
  // map has moved since, there is no place to report and none is invented.
  const served = state.artifacts.find((a) => a.tesseraId === opened.id);
  const parent =
    served && served.parentId !== null
      ? state.artifacts.find((a) => a.tesseraId === served.parentId)
      : undefined;
  const children = state.artifacts.filter((a) => a.parentId === opened.id).length;
  return panel(
    'Cluster',
    `${row('layer', opened.layer)}
     ${row('key', opened.key ?? '— none supplied —')}
     ${row('tessera_id', opened.id.toString())}
     ${served ? row('inside', parent ? describe(parent) : '— nothing you were served —') : ''}
     ${served && children > 0 ? row('holds', `${fmt(children)} served below it`) : ''}
     <div class="headline">${fmt(opened.maskedCount)} member${
       opened.maskedCount === 1n ? '' : 's'
     } you can see</div>
     <div class="muted">the number the map already carried, from the same predicate. There is no
       membership and no declared size behind it.</div>`
  );
}
