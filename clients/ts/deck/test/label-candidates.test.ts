import {describe, expect, it} from 'vitest';
import {type Artifact, type ArtifactsProjection, type Meta} from '@tesseradb/client';
import {SessionArtifactTable, attachedTextOf, servedLineage} from '@tesseradb/client/internal';
import {LayerManager, OrthographicView, type Layer} from '@deck.gl/core';
import {LABEL_CANDIDATE_CEILING, TesseraLayer, frontier, labelBudget, labelCandidates, type TesseraLayerInternalProps} from '../src/layer.js';
import {fakeDevice} from './fake-device.js';
import {LABEL_SIZE_MAX, LABEL_SIZE_MIN, placeLabels} from '../src/labels.js';
import medcpt from './fixtures/medcpt-kmeans-labels.json' with {type: 'json'};

/**
 * Which artifacts get a label: the frontier of the served set with text to draw, the top N by
 * masked count, each sized by that count on a logarithmic band over the frontier's range.
 */

const artifact = (id: bigint, count: bigint, content: string[] = [], layer = 'clusters', parent: bigint | null = null): Artifact => ({
  layer,
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: count,
  centroid: [Number(id) * 2 ** 24, Number(id) * 2 ** 24],
  box: null,
  content,
  parentIds: parent === null ? [] : [parent],
  rung: 0,
  matched: null,
  highlighted: null,
  target: null,
  slot: null
});

/**
 * A dependent layer's artifact, a topic label, naming its target by identifier. Its count is its
 * own and is not drawn.
 */
const topic = (id: bigint, target: bigint, text: string, count = 1n): Artifact => ({
  ...artifact(id, count, [text], 'topics'),
  target
});

/**
 * The served set as the server delivers it: on this treed fixture `rung` is the response-local
 * parent-chain depth, computed here as the server computes it (a root, and a child of an unserved
 * parent, at 0). Nothing under test derives it again.
 */
function withRungs(served: Artifact[]): Artifact[] {
  const byId = new Map(served.map((a) => [a.tesseraId, a]));
  const depthOf = (a: Artifact, guard = 0): number => {
    const parent = a.parentIds.length === 0 ? undefined : byId.get(a.parentIds[0]!);
    return parent && guard < 1024 ? depthOf(parent, guard + 1) + 1 : 0;
  };
  return served.map((a) => ({...a, rung: depthOf(a)}));
}

function projection(input: Artifact[]): ArtifactsProjection {
  const served = withRungs(input);
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentIds: a.parentIds, rung: a.rung})));
  return {layer: 'clusters', layers: ['clusters'], served, colourServed: [], lineage: servedLineage(served), attached: attachedTextOf(served, ['clusters', 'topics'].map((name) => ({name, hierarchy: {kind: 'flat' as const, pruneChildren: false}}))), status: 'shown', refusal: null, version: 1, held: 0, table, servedOrdinals: new Set(ordinals), shapes: new Map(), colours: new Map(), palette: 'tableau10', overrides: new Map(), coverage: {current: 0, stale: 0}};
}

const META = {layers: [{name: 'clusters', hierarchy: {kind: 'flat', pruneChildren: false}, depsOn: []}, {name: 'topics', hierarchy: {kind: 'flat', pruneChildren: false}, depsOn: ['clusters']}]} as unknown as Meta;

const ids = (s: Iterable<bigint>) => [...s].map(String).sort();

/**
 * A served k-means clustering, one artifact per row of the fixture: a real compact layout, which
 * synthetic centroids do not reproduce.
 */
function medcptClusters(): Artifact[] {
  return (medcpt.clusters as [string, string, string, number, number][]).map(([id, count, text, x, y]) => ({
    ...artifact(BigInt(id), BigInt(count), text.length > 0 ? [text] : [], 'clusters/kmeans'),
    centroid: [x, y] as [number, number]
  }));
}

const MEDCPT_META = {layers: [{name: 'clusters/kmeans', hierarchy: {kind: 'flat', pruneChildren: false}, depsOn: []}]} as unknown as Meta;

describe('frontier', () => {
  it('is every served artifact with no served child — an ancestor of something drawn is not on it', () => {
    // A chain 1 → 2 → 3 and a sibling 4 under 2: the frontier is 3 and 4, not the chain above.
    const p = projection([artifact(1n, 900n), artifact(2n, 500n, [], 'clusters', 1n), artifact(3n, 300n, [], 'clusters', 2n), artifact(4n, 200n, [], 'clusters', 2n)]);
    expect(ids(frontier(p, undefined))).toEqual(['3', '4']);
  });

  it('a flat layer is all frontier — nothing has a served child', () => {
    const p = projection([artifact(1n, 9n), artifact(2n, 8n), artifact(3n, 7n)]);
    expect(ids(frontier(p, undefined))).toEqual(['1', '2', '3']);
  });

  it('an artifact whose parent was withheld is a root, and a leaf if it has no served child', () => {
    const p = projection([artifact(1n, 9n), artifact(7n, 8n, [], 'clusters', 99n)]);
    expect(ids(frontier(p, undefined))).toEqual(['1', '7']);
  });

  it('at a chosen level it is that level and every branch that stopped above it', () => {
    // 1 → 2 → 3 is three deep; 4 is a child of 1 and stops there. At level 1 the frontier is 2
    // (the level) and 4 (a branch the level did not reach), not 1, whose child 2 is drawn.
    const p = projection([artifact(1n, 900n), artifact(2n, 500n, [], 'clusters', 1n), artifact(3n, 300n, [], 'clusters', 2n), artifact(4n, 200n, [], 'clusters', 1n)]);
    expect(ids(frontier(p, 1))).toEqual(['2', '4']);
    expect(ids(frontier(p, 0))).toEqual(['1']);
    expect(ids(frontier(p, 9))).toEqual(['3', '4']);
  });
});

describe('labelCandidates', () => {
  it('an artifact with no text draws no label — never its key', () => {
    const p = projection([artifact(1n, 100n, ['quantum error correction']), artifact(2n, 900n), artifact(3n, 50n, [''])]);
    const {candidates, byId} = labelCandidates(p, META, undefined, 0, 10);
    expect(candidates.map((c) => String(c.id))).toEqual(['1']);
    // One line, and the whole name is there.
    expect(byId.get(1n)!.line).toBe('quantum error correction');
    expect([...byId.values()].some((t) => t.line.startsWith('c-'))).toBe(false);
  });

  it('only the frontier is labelled — an ancestor of something drawn draws nothing', () => {
    const p = projection([
      artifact(1n, 900n, ['the whole corpus']),
      artifact(2n, 500n, ['a big split'], 'clusters', 1n),
      artifact(3n, 300n, ['a small split'], 'clusters', 2n),
      artifact(4n, 200n, ['another small split'], 'clusters', 2n)
    ]);
    const {candidates} = labelCandidates(p, META, undefined, 0, 10);
    expect(candidates.map((c) => String(c.id))).toEqual(['3', '4']);
  });

  it('under a filter, an artifact with no matching member in view draws no label', () => {
    const p = projection([
      {...artifact(1n, 900n, ['nothing matches here']), matched: false},
      {...artifact(2n, 100n, ['something matches here']), matched: true}
    ]);
    expect(labelCandidates(p, META, undefined, 0, 10).candidates.map((c) => String(c.id))).toEqual(['2']);
    // A parent that matches, whose children all match nothing, is named in their place.
    const parent = projection([
      {...artifact(1n, 900n, ['the parent']), matched: true},
      {...artifact(2n, 500n, ['a child'], 'clusters', 1n), matched: false},
      {...artifact(3n, 400n, ['another child'], 'clusters', 1n), matched: false}
    ]);
    expect(labelCandidates(parent, META, undefined, 0, 10).candidates.map((c) => String(c.id))).toEqual(['1']);
    // A filter matching nothing in view names nothing.
    const none = projection([{...artifact(1n, 900n, ['a']), matched: false}, {...artifact(2n, 100n, ['b']), matched: false}]);
    expect(labelCandidates(none, META, undefined, 0, 10).candidates).toEqual([]);
  });

  it('a nameless cluster with a topic attached takes the topic as its name', () => {
    const p = projection([artifact(1n, 100n), artifact(2n, 40n), topic(9n, 1n, 'decoders, thresholds')]);
    const {candidates, byId} = labelCandidates(p, META, undefined, 0, 10);
    expect(candidates.map((c) => String(c.id))).toEqual(['1']);
    expect(byId.get(1n)!.line).toBe('decoders, thresholds');
    expect(byId.get(1n)!.topic).toBeNull();
  });

  it('attaches each topic to the cluster its `target` names, where two clusters hold the same count', () => {
    // Both clusters have 100 visible members; the join is by `target`, so equal counts do not
    // matter.
    const p = projection([artifact(1n, 100n), artifact(2n, 100n), topic(8n, 1n, 'left topic'), topic(9n, 2n, 'right topic')]);
    const {byId} = labelCandidates(p, META, undefined, 0, 10);
    expect(byId.get(1n)!.line).toBe('left topic');
    expect(byId.get(2n)!.line).toBe('right topic');
    // And a topic naming nothing this response holds attaches to nothing rather than to whichever
    // row happens to share its count.
    const orphan = projection([artifact(1n, 100n), {...topic(7n, 1n, 'unattached'), target: null}]);
    expect(labelCandidates(orphan, META, undefined, 0, 10).byId.get(1n)).toBeUndefined();
  });

  it('size is the masked count, and level says nothing: the deeper, larger name draws larger', () => {
    // 2 stops at depth 1 with 400 members; 5 and 6 are a level deeper and 5 is the biggest thing
    // drawn. Size follows the counts, not the depths.
    const p = projection([
      artifact(1n, 9000n, ['root']),
      artifact(2n, 400n, ['stops here'], 'clusters', 1n),
      artifact(3n, 5000n, ['an ancestor'], 'clusters', 1n),
      artifact(5n, 4000n, ['deeper and bigger'], 'clusters', 3n),
      artifact(6n, 100n, ['deeper and smaller'], 'clusters', 3n)
    ]);
    const {byId} = labelCandidates(p, META, undefined, 0, 10);
    expect([...byId.keys()].map(String).sort()).toEqual(['2', '5', '6']);
    // The largest count on screen is the largest name on screen, whatever level it sits at.
    expect(byId.get(5n)!.size).toBeCloseTo(LABEL_SIZE_MAX, 6);
    expect(byId.get(6n)!.size).toBeCloseTo(LABEL_SIZE_MIN, 6);
    expect(byId.get(2n)!.size).toBeGreaterThan(byId.get(6n)!.size);
    expect(byId.get(2n)!.size).toBeLessThan(byId.get(5n)!.size);
  });

  it('spans the band over the counts drawn — the smallest at the floor, the largest at the top', () => {
    const served = Array.from({length: 5}, (_, i) => artifact(BigInt(i + 1), BigInt(10 ** (5 - i)), [`cluster ${i}`]));
    const {byId} = labelCandidates(projection(served), META, undefined, 0, 10);
    const sizes = [...byId.values()].map((t) => t.size);
    expect(Math.max(...sizes)).toBeCloseTo(LABEL_SIZE_MAX, 6);
    expect(Math.min(...sizes)).toBeCloseTo(LABEL_SIZE_MIN, 6);
    // Four decades over the band, so the decades are even steps.
    const ordered = [...byId.entries()].sort((a, b) => Number(a[0] - b[0])).map(([, t]) => t.size);
    for (let i = 1; i < ordered.length - 1; i++) {
      expect(ordered[i]! - ordered[i + 1]!).toBeCloseTo(ordered[i - 1]! - ordered[i]!, 6);
    }
  });

  it('a frontier with no range lands on the band rather than dividing by zero', () => {
    // One name on screen, and every count equal: both take the top of the band, and neither is NaN.
    const one = labelCandidates(projection([artifact(1n, 4812n, ['the only cluster'])]), META, undefined, 0, 10);
    expect(one.byId.get(1n)!.size).toBe(LABEL_SIZE_MAX);
    const flat = Array.from({length: 4}, (_, i) => artifact(BigInt(i + 1), 500n, [`cluster ${i}`]));
    const {byId} = labelCandidates(projection(flat), META, undefined, 0, 10);
    const sizes = [...byId.values()].map((t) => t.size);
    expect(sizes).toEqual([LABEL_SIZE_MAX, LABEL_SIZE_MAX, LABEL_SIZE_MAX, LABEL_SIZE_MAX]);
    // And an empty frontier asks nothing of the band at all.
    expect(labelCandidates(projection([]), META, undefined, 0, 10).candidates).toEqual([]);
  });

  it('takes the top N by masked count, and the placement then drops what overlaps', () => {
    const served = Array.from({length: 30}, (_, i) => artifact(BigInt(i + 1), BigInt(1000 - i), [`cluster ${i}`]));
    const p = projection(served);
    const {candidates} = labelCandidates(p, META, undefined, 0, 12);
    expect(candidates.length).toBe(12);
    expect(candidates.map((c) => Number(c.id))).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    expect(candidates[0]!.priority).toBe(1000);
    // Every candidate has a box the spatial hash can refuse.
    expect(candidates.every((c) => c.width > 0 && c.height > 0)).toBe(true);
    // Thirty of them on one spot: the placement keeps some and drops the rest, no two overlapping.
    const stacked = labelCandidates(p, META, undefined, 0, 30).candidates.map((c) => ({...c, x: 100, y: 100}));
    const placed = placeLabels(stacked);
    expect(placed.length).toBeLessThan(stacked.length);
    expect(placed.length).toBeGreaterThan(0);
  });

  it('a zero budget places nothing', () => {
    const p = projection([artifact(1n, 100n, ['a'])]);
    expect(labelCandidates(p, META, undefined, 0, 0).candidates).toEqual([]);
  });
});

/**
 * A zoom scales the anchors and nothing else, so the sorted, budgeted list is built once per
 * served set and each zoom bucket, a quarter of a level, scales a copy. Building the list costs
 * far more than placing it.
 */
describe('the candidate list is held per served set, not per zoom bucket', () => {
  const set = [artifact(1n, 900n, ['first']), artifact(2n, 500n, ['second']), artifact(3n, 300n, ['third'])];

  it('is the same list at every zoom, with the anchors scaled', () => {
    const p = projection(set);
    const at9 = labelCandidates(p, META, undefined, 9, 10);
    const at925 = labelCandidates(p, META, undefined, 9.25, 10);
    // The text, sizes and boxes are one object shared by every bucket: nothing about them is a
    // function of zoom.
    expect(at925.byId).toBe(at9.byId);
    expect(at925.candidates.map((c) => String(c.id))).toEqual(at9.candidates.map((c) => String(c.id)));
    expect(at925.candidates.map((c) => c.width)).toEqual(at9.candidates.map((c) => c.width));
    // The anchors are pixels, and a quarter of a zoom level is a factor of 2^0.25.
    for (const [i, c] of at925.candidates.entries()) {
      expect(c.x).toBeCloseTo(at9.candidates[i]!.x * 2 ** 0.25, 6);
      expect(c.y).toBeCloseTo(at9.candidates[i]!.y * 2 ** 0.25, 6);
    }
    // Each bucket gets its own candidate objects: scaling in place would move the held list.
    expect(at925.candidates[0]).not.toBe(at9.candidates[0]);
  });

  it('is rebuilt when the served set, the level or the budget moves', () => {
    const p = projection(set);
    const first = labelCandidates(p, META, undefined, 9, 10).byId;
    expect(labelCandidates(p, META, undefined, 9, 2).byId).not.toBe(first);
    expect(labelCandidates(p, META, 0, 9, 10).byId).not.toBe(first);
    // A response replacing the served set under the same object identity makes a new list.
    expect(labelCandidates({...p, version: p.version + 1}, META, undefined, 9, 10).byId).not.toBe(first);
  });
});

describe('labelBudget', () => {
  it('is the drawn set, up to the work ceiling', () => {
    expect(labelBudget(253)).toBe(253);
    expect(labelBudget(0)).toBe(0);
    expect(labelBudget(23_821)).toBe(LABEL_CANDIDATE_CEILING);
  });

  /**
   * 253 k-means clusters as served to the widest principal (`fixtures/medcpt-kmeans-labels.json`),
   * in a ball 464 world units across. Offering the top 28 (one per 36,000 px² of a 1280 × 800
   * screen) places few, since they compete for the same ground. The check is the ratio between the
   * two, since the figures move with the font metrics.
   */
  it('names many more of a compact layout than a screen-derived budget did', () => {
    const p = projection(medcptClusters());
    const placed = (budget: number) => placeLabels(labelCandidates(p, MEDCPT_META, undefined, 0, budget).candidates).length;
    const screenDerived = placed(28);
    const drawnSet = placed(labelBudget(p.served.length));
    expect(screenDerived).toBeLessThanOrEqual(8);
    expect(drawnSet).toBeGreaterThanOrEqual(3 * screenDerived);
  });

  it('offers every drawn artifact, so a small cluster in a gap is a candidate at all', () => {
    const p = projection(medcptClusters());
    const {candidates} = labelCandidates(p, MEDCPT_META, undefined, 0, labelBudget(p.served.length));
    expect(candidates).toHaveLength(253);
  });
});

describe('labels at rest and under the pointer', () => {
  /** The rows of each text sublayer, drawn by deck's own LayerManager over an orthographic viewport. */
  function drawn(hovered: bigint | null) {
    const manager = new LayerManager(fakeDevice(), {});
    manager.activateViewport(new OrthographicView({flipY: true}).makeViewport({width: 800, height: 600, viewState: {target: [256, 256, 0], zoom: 0}})!);
    const p = projection([artifact(1n, 100n), artifact(2n, 90n, ['graph neural networks']), topic(9n, 1n, 'decoders, thresholds')]);
    manager.setLayers([new TesseraLayer({id: 'tessera', depth: 2, status: 'shown', artifacts: p, meta: META, hoveredArtifact: hovered} as TesseraLayerInternalProps)]);
    const layer = manager.getLayers().find((l) => l.id === 'tessera') as TesseraLayer;
    const rows = (id: string) => {
      const sub = (layer.getSubLayers() as Layer[]).find((l) => l.id === `tessera-${id}`);
      return ((sub?.props.data as {text: string; id: bigint}[] | undefined) ?? []).map((d) => [String(d.id), d.text]);
    };
    return {names: rows('labels'), counts: rows('label-counts')};
  }

  it('draws every name at rest, and a count only for the artifact under the pointer', () => {
    const rest = drawn(null);
    expect(rest.names.map(([id]) => id).sort()).toEqual(['1', '2']);
    expect(rest.counts).toEqual([]);
    expect(drawn(2n).counts).toEqual([['2', '90']]);
  });
});
