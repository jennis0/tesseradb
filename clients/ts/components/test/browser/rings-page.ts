import {Deck, OrthographicView} from '@deck.gl/core';
import type {ArtifactsProjection, Band, LegendProjection, MarksProjection, Meta} from '@tesseradb/client';
import {SessionArtifactTable, servedLineage} from '@tesseradb/client/internal';
import {TesseraLayer, resolvePick} from '@tesseradb/deck';

// Built by `rings.browser.ts` with Vite from the packages' sources. Two marks sized by `c`: one
// with the value 10 at world (156, 256), one with no value at (356, 256), which draws as a ring. The
// 400 px canvas shows the world at zoom 0 about its centre, so they sit at (100, 200) and (300, 200).
const band = {
  depth: 2,
  prefix: 0n,
  x: 0,
  y: 0,
  ids: BigUint64Array.of(11n, 22n),
  positions: Float32Array.of(156, 256, 356, 256),
  scalars: {c: {arrowType: 'u32', values: Uint32Array.of(10, 0), present: Uint8Array.of(1, 0)}},
  served: 2,
  capUsed: 500,
  visible: 2n,
  matched: 2n,
  highlighted: 2n,
  highlightBits: null,
  membership: {},
  heldBelow: 23n,
  identityKey: 'rings',
  contentKey: 'rings',
  bytes: 64,
  touchedAt: 0
} as unknown as Band;
const meta = {declaredScalars: [{name: 'c', arrowType: 'u32', category: null, render: true, index: false, unique: false, analyser: null, homes: ['rendered']}], layers: []} as unknown as Meta;
const legend: LegendProjection = {ranks: {}, domains: {c: {min: 0, max: 10}}, samples: {}, missing: {c: true}, categories: {}, categoryErrors: {}, colourBy: null, sizeBy: 'c'};
const marks: MarksProjection = {bands: [band], standIn: [], count: {shown: 2, total: 2, exact: true}};
// No layer is served; the layer still binds its colour lookup texture, which the marks' program reads.
const artifacts: ArtifactsProjection = {
  layer: null,
  layers: [],
  served: [],
  colourServed: [],
  attached: new Map(),
  lineage: servedLineage([]),
  status: 'idle',
  refusal: null,
  version: 0,
  held: 0,
  table: new SessionArtifactTable(),
  servedOrdinals: new Set(),
  shapes: new Map(),
  colours: new Map(),
  palette: 'positional',
  coverage: {current: 0, stale: 0}
};

const parent = document.createElement('div');
parent.style.cssText = 'position:relative;width:400px;height:400px';
document.body.append(parent);

let drawn = 0;
let read: Record<string, number> | null = null;
const deck = new Deck({
  parent,
  width: 400,
  height: 400,
  useDevicePixels: false,
  views: new OrthographicView({flipY: true}),
  initialViewState: {target: [256, 256, 0], zoom: 0},
  layers: [
    new TesseraLayer({
      id: 'tessera',
      depth: 2,
      status: 'shown',
      meta,
      legend,
      marks,
      artifacts,
      density: 'none',
      labels: false,
      sizing: {min: 8, max: 20, scale: 'linear'},
      pointOpacity: 1,
      onDrawn: (n: number) => (drawn = n)
    })
  ],
  onAfterRender: () => {
    if (drawn === 0) return;
    // Read the frame just drawn, before the browser presents and clears it.
    const canvas = parent.querySelector('canvas')!;
    const copy = document.createElement('canvas');
    copy.width = canvas.width;
    copy.height = canvas.height;
    const ctx = copy.getContext('2d')!;
    ctx.drawImage(canvas, 0, 0);
    const alpha = (x: number, y: number) => ctx.getImageData(x, y, 1, 1).data[3]!;
    read = {valued: alpha(100, 200), ringCentre: alpha(300, 200), ringEdge: alpha(307, 200), outside: alpha(312, 200)};
  }
});

// The first frames can go out before the programs link; draw again until the marks show.
let tries = 0;
const settle = () => {
  if ((read && read['valued']! > 0) || ++tries > 120) {
    const picked = (x: number) => {
      const p = resolvePick((deck.pickObject({x, y: 200, radius: 0}) ?? {index: -1}) as never);
      return p.kind === 'mark' ? String(p.id) : p.kind;
    };
    (window as unknown as {result: unknown}).result = {...read, pickCentre: picked(300), pickEdge: picked(307), pickOutside: picked(312)};
    return;
  }
  deck.redraw('rings');
  requestAnimationFrame(settle);
};
requestAnimationFrame(settle);
