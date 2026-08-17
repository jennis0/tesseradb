import {OrthographicView, type BinaryAttribute as DeckBinaryAttribute, type Layer} from '@deck.gl/core';
import {ScatterplotLayer, TextLayer} from '@deck.gl/layers';
import {CELLS_PER_WORLD_UNIT, MAX_DEPTH, WORLD_SIZE} from '@tessera/client';
import {placedArtifacts, type ArtifactPlaces} from './artifacts.js';
import {assertAssemblyMatchesServed, type Assembled} from './assemble.js';
import {buildColourAttribute, type Encoding} from './colour.js';
import {readConfig} from './config.js';
import {MarkSlab, type GpuSlab} from './slab.js';
import {trace} from './trace.js';
import type {Store} from './state.js';

/** The two debug knobs — see {@link ViewerConfig}. Read once: neither changes within a session. */
const RENDER = readConfig();

export const VIEW = new OrthographicView({id: 'ortho', flipY: true});

export const INITIAL_VIEW_STATE = {
  target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0] as [number, number, number],
  zoom: 0,
  minZoom: -2,
  maxZoom: MAX_DEPTH
};


export type ViewState = {
  target: [number, number, number];
  zoom: number;
};


function worldToDataBbox(
  world: [number, number, number, number],
  q: {xMin: number; xMax: number; yMin: number; yMax: number}
): [number, number, number, number] {
  const sx = (q.xMax - q.xMin) / WORLD_SIZE;
  const sy = (q.yMax - q.yMin) / WORLD_SIZE;
  return [
    q.xMin + world[0] * sx,
    q.yMin + world[1] * sy,
    q.xMin + world[2] * sx,
    q.yMin + world[3] * sy
  ];
}

/**
 * The current colour encoding, resolved from state.
 *
 * **Falls back to uniform rather than throwing** at every step where the state is not yet ready —
 * a column chosen before its values have resolved, a refused `/v1/categories`. Colour is
 * presentation, so an incomplete encoding must degrade to a drawn map, never to no map.
 */
/**
 * What the current colouring *is*, as a string — the paint key's colour half.
 *
 * **The encoding changes without anything else changing.** Resolving `/v1/categories` moves the
 * ranks and nothing else: same marks, same depth, same store version. A repaint condition that
 * omitted this skipped the paint that would have applied the palette, so the map stayed uniform
 * until an unrelated change forced a redraw — which is what a zoom is.
 *
 * Sizes rather than contents, for the reason {@link MarkSlab} compares them that way: ranks and
 * domains are sticky accumulators that only ever grow.
 */
export function encodingSignature(store: Store): string {
  const encoding = encodingOf(store);
  switch (encoding.kind) {
    case 'uniform':
    case 'unmapped':
      return encoding.kind;
    case 'category':
      return `category|${encoding.column}|${Object.keys(encoding.rankOfCode).length}`;
    case 'numeric':
      return `numeric|${encoding.column}|${encoding.domain.min}|${encoding.domain.max}`;
  }
}

function encodingOf(store: Store): Encoding {
  const {colourBy, meta, categoryErrors, ranks, domains} = store.state;
  if (!colourBy || !meta) return {kind: 'uniform'};
  const column = meta.declaredScalars.find((c) => c.name === colourBy);
  if (!column) return {kind: 'uniform'};

  // A refused column colours every mark unmapped, not uniform. The distinction is the whole point:
  // uniform means "no encoding chosen", unmapped means "this value could not be named" — and the
  // legend says the latter, so the map must not quietly show the former.
  if (categoryErrors[colourBy]) return {kind: 'unmapped'};

  if (column.category) {
    // **Paint follows the ranks, and the ranks are local.** A colour is `colourOfRank`, and a rank
    // is assigned from the codes counted in the bands already held — no round trip is involved. So
    // the map is painted the moment the count lands, and does not wait for `/v1/categories`.
    //
    // Waiting for it was a mistake, and a visible one: the resolved value list is needed to *name* a
    // colour in the legend and for nothing else, so gating the paint on it left every mark uniform
    // across a fetch, and the map changed colour in one jump when the reply arrived. Worse at scale
    // — the 25M bundle streams bands for seconds before the count runs at all.
    //
    // It discloses nothing extra. The codes being ranked came from marks the mask already admitted,
    // which is the same argument that lets the legend exist; what stays gated is the *name*, and the
    // legend still shows only values `/v1/categories` was willing to return.
    const rankOfCode = ranks[colourBy];
    if (!rankOfCode || Object.keys(rankOfCode).length === 0) return {kind: 'uniform'};
    return {kind: 'category', column: colourBy, rankOfCode};
  }
  const domain = domains[colourBy];
  if (!domain) return {kind: 'uniform'};
  return {kind: 'numeric', column: colourBy, domain};
}

/**
 * **Stand-in marks are drawn at full alpha, like any other mark.**
 *
 * They were faded to 45% so that a superset of what the definition serves could not be mistaken for
 * the answer. Measured against the thing being optimised for, that was the wrong trade: the fade is
 * the *main remaining signature of pop-in*, because the transition a reader notices is not marks
 * appearing — the stand-in already put them there — but the same marks jumping from half to full
 * opacity when the exact band lands. Removing it removes the visible event.
 *
 * What guards the reading is unchanged, and it is the part that was ever normative: **no
 * number-channel value is shown against a non-exact tile** (`delta-serving.md` §7). The count
 * channel, not the alpha channel, is what stops a superset being read as density; the panel reports
 * how many marks are provisional, and nothing displays a figure derived from them. The alpha was a
 * courtesy, and it cost more than it bought.
 */

/**
 * The stand-in colour buffer, reused while the stand-ins and the encoding are the same.
 *
 * The layers are rebuilt on every paint, and a paint happens per store change — arrivals, absorb
 * slices, panel updates. The stand-in *marks* survive most of those by reference (`refreshExact`
 * keeps `standIn` whole for exactly this reason), but their colour buffer was rebuilt from scratch
 * each time: 5.8 x 10^5 marks at the median and 2.7 x 10^6 at worst, measured at 50–70 ms of the
 * very frames that missed p95. Keyed on the stand-in object and the encoding signature — the same
 * two things that decide its contents — with a `WeakMap` so a departed frame frees its buffer.
 */
const heldStandInColours = new WeakMap<object, {key: string; colours: Uint8Array}>();

/** Frames whose fidelity checks have run — once per frame object, see the call site. */
const checkedFrames = new WeakSet<object>();

/**
 * Per-drawn-tile density between consecutive frames at one depth — the instrument for banding.
 *
 * A visual artifact is a **tile whose on-screen density moves**, and nothing else in the trace can
 * see one: the aggregates say how many marks a frame drew, not that one patch flipped from 40
 * marks to 8 when its stand-in was replaced. Each frame's tiles are folded onto the drawn depth's
 * grid (descendants project up; ancestors spread over a block and are skipped as unattributable)
 * and compared with the previous frame's grid. The transition worth the most is provisional →
 * exact, where the new count is the server's own answer — so `med`/`pops` measure exactly how
 * wrong the stand-ins were about the ground they covered, in the units a viewer perceives.
 *
 * Trace-only (`?trace=1`): the fold is a map of ~10^4–10^5 entries per derived frame.
 */
type DensityCell = {drawn: number; exact: boolean; exactDrawn: number; standIn: number; maxSrc: number};
let lastDensity: {depth: number; tiles: Map<bigint, DensityCell>} | null = null;

function auditDensity(assembled: Assembled): void {
  const tiles = new Map<bigint, DensityCell>();
  for (const t of assembled.tiles) {
    if (t.depth < assembled.depth) continue;
    const key = t.depth === assembled.depth ? t.prefix : t.prefix >> BigInt(2 * (t.depth - assembled.depth));
    let held = tiles.get(key);
    if (!held) {
      held = {drawn: 0, exact: false, exactDrawn: 0, standIn: 0, maxSrc: assembled.depth};
      tiles.set(key, held);
    }
    held.drawn += t.drawn;
    if (t.exact) {
      held.exact = true;
      held.exactDrawn += t.drawn;
    } else {
      held.standIn += t.drawn;
      if (t.depth > held.maxSrc) held.maxSrc = t.depth;
    }
  }
  const prev = lastDensity;
  lastDensity = {depth: assembled.depth, tiles};
  if (!prev || prev.depth !== assembled.depth) return;

  let compared = 0;
  let up = 0;
  let down = 0;
  let worst = 1;
  const pops: number[] = [];
  let worstCell: {r: number; was: DensityCell; cur: DensityCell} | null = null;
  for (const [key, cur] of tiles) {
    const was = prev.tiles.get(key);
    if (!was || was.drawn === 0 || cur.drawn === 0) continue;
    compared++;
    const r = cur.drawn / was.drawn;
    if (r > 2) up++;
    else if (r < 0.5) down++;
    if (r > worst) worst = r;
    if (1 / r > worst) worst = 1 / r;
    if ((worstCell?.r ?? 0) < Math.max(r, 1 / r)) worstCell = {r: Math.max(r, 1 / r), was, cur};
    if (!was.exact && cur.exact) pops.push(r);
  }
  if (compared === 0) return;
  // A burst names its worst offender: how the tile's marks decompose on each side of the jump —
  // exact vs stand-in, and how deep the stand-ins' source bands sit. The aggregate said the
  // zoom-out overdraw exists; this says which mechanism supplied the excess marks.
  if ((up > 100 || worst > 4) && worstCell) {
    trace.event('overdraw', {
      depth: assembled.depth,
      r: Math.round(worstCell.r * 10) / 10,
      wasExact: worstCell.was.exactDrawn,
      wasStand: worstCell.was.standIn,
      curExact: worstCell.cur.exactDrawn,
      curStand: worstCell.cur.standIn,
      src: worstCell.cur.maxSrc - assembled.depth
    });
  }
  pops.sort((a, b) => a - b);
  trace.event('density', {
    depth: assembled.depth,
    n: compared,
    up,
    down,
    pops: pops.length,
    // Median provisional→exact ratio, ×100: 100 means the stand-ins matched the served density.
    med: pops.length > 0 ? Math.round(pops[pops.length >> 1]! * 100) : 0,
    worst: Math.round(worst * 10) / 10
  });
}

function standInColours(assembled: Assembled, encoding: Encoding, key: string): Uint8Array {
  const held = heldStandInColours.get(assembled.standIn);
  if (held && held.key === key) return held.colours;
  const colours = buildColourAttribute(assembled.provisional, assembled.standIn.scalars, encoding);
  heldStandInColours.set(assembled.standIn, {key, colours});
  return colours;
}

/**
 * A binary attribute descriptor, reused for as long as its buffer is the same object.
 *
 * **deck.gl's skip check is reference equality on this descriptor, not on the array inside it.**
 * `Attribute.setBinaryValue` returns early on `state.binaryValue === buffer` — where `buffer` is
 * this `{value, size}` object — so building a fresh literal each paint misses the check every time
 * and re-uploads an attribute whose bytes have not changed. That defeated the slab entirely at the
 * last step: measured at the WebGL call level, one pan uploaded 32.9 MB where 16.4 MB was needed,
 * and 121 of 144 paints in a recorded session re-uploaded a 1.4 x 10^6-mark buffer they had not
 * touched.
 *
 * **Keyed on the array, so a republished buffer always uploads.** The slab returns a *new* subarray
 * whenever its contents or extent change and the identical one when they have not, which is exactly
 * the signal wanted here — a `WeakMap` turns that into descriptor identity without keeping a buffer
 * alive or needing an invalidation rule of its own.
 *
 * This is narrower than memoising the whole `data` object, which is the change that rendered
 * rectangles of the view black: `data` identity suppresses `dataChanged` and with it every
 * downstream invalidation, including the picking colours. Here `dataChanged` still fires and only
 * the two binary attributes take deck's own documented skip.
 */
type BinaryAttribute<T extends ArrayBufferView> = {value: T; size: number; normalized?: boolean};
const descriptors = new WeakMap<ArrayBufferView, BinaryAttribute<ArrayBufferView>>();

function binary<T extends ArrayBufferView>(
  value: T,
  size: number,
  normalized?: boolean
): BinaryAttribute<T> {
  let held = descriptors.get(value);
  if (!held) {
    held = normalized === undefined ? {value, size} : {value, size, normalized};
    descriptors.set(value, held);
  }
  return held as BinaryAttribute<T>;
}

/**
 * Attribute descriptors around a partition's own GPU buffers.
 *
 * **Keyed by attribute name, not accessor name** — `data.attributes.instancePositions` routes to
 * `Attribute.setExternalBuffer`, which binds the buffer and uploads nothing; `getPosition` would
 * route to `setBinaryValue`, deck's own copy-and-upload path, which is the cost being removed. The
 * accessor shapes match what the typed-array path produced exactly — positions `float32 ×2`
 * stride 8 (the fp64 low half stays a disabled constant, as it is for an f32 typed array), colours
 * `unorm8 ×4` — so the shader sees identical bytes either way.
 *
 * Memoised on the {@link GpuSlab} object, which the partition keeps stable across appends: an
 * unchanged partition hands deck the identical descriptor, and `setExternalBuffer` returns on
 * reference equality before doing anything at all. Span writes happened at absorb time, in the
 * slab; by the time deck sees the frame there is nothing left to move.
 */
type AttributeMap = Record<string, DeckBinaryAttribute>;
const gpuDescriptors = new WeakMap<GpuSlab, AttributeMap>();

function gpuAttributes(gpu: GpuSlab): AttributeMap {
  let held = gpuDescriptors.get(gpu);
  if (!held) {
    held = {
      instancePositions: {buffer: gpu.positions, size: 2, type: 'float32', stride: 8, offset: 0},
      instanceFillColors: {buffer: gpu.colours, size: 4, type: 'unorm8', stride: 4, offset: 0},
      // Written once at buffer creation — the values depend only on the instance index, so deck's
      // per-data-change regeneration and 4n-byte re-upload are both skipped. See `slab.ts`.
      instancePickingColors: {buffer: gpu.picking, size: 4, type: 'uint8', stride: 4, offset: 0}
    };
    gpuDescriptors.set(gpu, held);
  }
  return held;
}

/**
 * The mark layers: served marks from the slab, stand-in marks beside them.
 *
 * **Every served mark is drawn.** The length handed to deck.gl is the resident count,
 * unconditionally — no budget, no cap, no filter applies here.
 *
 * **Two layers, not one, and not one per tile.** They exist because the two sets have different
 * lifetimes, not because they are drawn differently: exact bands accumulate and are retained across
 * frames, while stand-ins are re-clipped whenever a response lands. Splitting them is what lets the
 * first be written once. It also makes the fade uniform over a whole buffer rather than a per-tile
 * walk over ranges the slab no longer has.
 */
export function buildViewportLayers(store: Store, slab: MarkSlab): Layer[] {
  const {assembled, selectedWorldXY, status} = store.state;
  const layers: Layer[] = [];

  // A refusal draws no marks but keeps the slab: the held bands are still the answer to the last
  // view that succeeded, and discarding them would make recovery pay for a full rewrite.
  if (status === 'refused') return selectionLayers(selectedWorldXY);
  if (!assembled) {
    slab.clear();
    return selectionLayers(selectedWorldXY);
  }

  const encoding = encodingOf(store);
  const before = slab.drawn;
  const marks = trace.phase(
    'slab',
    () => slab.sync(assembled.bands, assembled.depth, encoding, store.state.colourBy),
    {n: assembled.bands.length}
  );
  // Whether deck.gl is handed the same buffers it already holds is the whole question for upload
  // cost, and it is invisible from outside — so it is recorded rather than inferred.
  trace.event('marks', {n: marks.length, added: marks.length - before, standIn: assembled.provisional});
  // **The fidelity checks run once per frame, not once per paint.** Both walk every band —
  // O(10^4-10^5) — and a paint happens on every store change, most of which change no band: the
  // pair measured as a real share of `layers` time doing the same arithmetic on the same objects.
  // A frame is immutable once assembled, and the slab was synced against it in the line above, so
  // one pass per frame object is the same guarantee at a fraction of the cost.
  if (!checkedFrames.has(assembled)) {
    checkedFrames.add(assembled);
    if (trace.enabled) auditDensity(assembled);
    assertAssemblyMatchesServed(assembled);
    // Every exact band the frame draws must have reached the slab: a band written outside its slot,
    // or a slot gone stale under a partition change, would otherwise thin the picture in a way
    // nothing else notices.
    for (const band of assembled.bands) {
      if (!slab.holds(band)) {
        throw new Error(
          `assembly: exact band ${band.prefix} at depth ${band.depth} is drawn but has no slab slot.`
        );
      }
    }
  }

  // **Layers toggle `visible`; they are never omitted.** deck.gl retains a layer's buffers across
  // renders by matching `id` — a layer absent from one render is destroyed, and re-adding it
  // regenerates and re-uploads everything it held. The stand-in layer flips between empty and not
  // on every coverage change, which made each flip a full re-upload of up to 2.7 x 10^6 marks.
  // (deck.gl performance guide: "favor layer visibility over addition/removal".)
  //
  // **One layer per retained slab partition, addressed by slot.** A depth flip swaps which
  // partition is visible; the other keeps its layer, its buffers and its GPU residency, so
  // flipping back uploads nothing. The slot number is the layer id precisely because it is stable
  // for a partition's whole life — an id derived from the depth would make eviction reshuffle
  // identities and re-upload both.
  for (const held of slab.layers()) {
    layers.push(
      new ScatterplotLayer({
        id: `marks-p${held.slot}`,
        visible: held.active && held.draw.length > 0,
        data: {
          length: held.draw.length,
          attributes: held.draw.gpu
            ? gpuAttributes(held.draw.gpu)
            : {
                getPosition: binary(held.draw.positions, 2),
                getFillColor: binary(held.draw.colours, 4, true)
              }
        },
        tesseraIds: held.draw.ids,
        radiusUnits: 'pixels' as const,
        getRadius: RENDER.radius,
        radiusMinPixels: 1,
        pickable: RENDER.pickable && held.active,
        parameters: {depthCompare: 'always' as const}
      })
    );
  }

  {
    // One entry per drawn mark by construction — see `buildColourAttribute`. Asserted anyway,
    // because a short buffer is the one way colour could silently drop marks: deck.gl reads
    // `length` from `data`, so a short attribute renders garbage rather than failing.
    const colours = standInColours(assembled, encoding, encodingSignature(store));
    if (colours.length !== assembled.provisional * 4) {
      throw new Error(
        `colour buffer covers ${colours.length / 4} of ${assembled.provisional} stand-in marks. ` +
          `Colour is presentation and must never decide what is drawn.`
      );
    }
    layers.push(
      new ScatterplotLayer({
        id: 'marks-standin',
        visible: assembled.provisional > 0,
        data: {
          length: assembled.provisional,
          attributes: {
            getPosition: binary(assembled.standIn.positions, 2),
            getFillColor: binary(colours, 4, true)
          }
        },
        tesseraIds: assembled.standIn.ids,
        radiusUnits: 'pixels' as const,
        getRadius: RENDER.radius,
        radiusMinPixels: 1,
        pickable: RENDER.pickable,
        parameters: {depthCompare: 'always' as const}
      })
    );
  }

  layers.push(...artifactLayers(store));
  layers.push(...selectionLayers(selectedWorldXY));
  return layers;
}

/**
 * The clusters the current view is served: a ring per artifact, with its masked count beside it.
 *
 * **Drawn from what the server served, joined to where the publisher said it was.** The two halves
 * come from different places and only one of them is authoritative: the identity and the count are
 * the service's, the position is the publish script's sidecar (`artifacts.ts`, `ArtifactPlaces`).
 * The join is one-directional on purpose — an artifact the response did not carry is not drawn,
 * whatever the sidecar holds — so the scaffolding can never put a cluster on screen that this
 * principal was not served.
 *
 * **The radius encodes the masked count, and the count is written beside it.** By square root, so
 * area is proportional to the count rather than radius: a ring four times the width of another
 * would otherwise read as four times the members while covering sixteen times the area. It is
 * still a comparison between *this principal's* counts — nothing on screen is a fraction of a
 * cluster's size, which is not a number this client is ever given.
 */
function artifactLayers(store: Store): Layer[] {
  const {artifacts, artifactPlaces, artifactLayer} = store.state;
  if (!artifactLayer || artifacts.length === 0) return [];
  // No sidecar for this layer means no position for any of its artifacts. They are still listed
  // with their counts in the panel — the count came from the service, and only the position did
  // not.
  const places = artifactPlaces.get(artifactLayer);
  if (!places) return [];
  const placed = placedArtifacts(artifacts, places);
  if (placed.length === 0) return [];

  const data = placed.map((p) => ({
    id: p.artifact.tesseraId,
    count: Number(p.artifact.maskedCount),
    // **The key, not the supplied text — the text belongs in the panel and was tried here.** Two
    // things defeat it on the map: twenty-two descriptions at eleven pixels cover the marks they
    // annotate, in the dense middle where the clusters are; and the text atlas is built over ASCII,
    // so any character outside it renders as a hole rather than as a glyph. The panel is HTML, has
    // room to wrap, and needs no atlas.
    label: p.artifact.stableKey ?? `#${p.artifact.tesseraId}`,
    position: [p.x / CELLS_PER_WORLD_UNIT, p.y / CELLS_PER_WORLD_UNIT] as [number, number]
  }));
  // Against the largest count *in this view for this principal*, so the smallest cluster is still
  // visible when every count is small. It rescales as the mask changes, which is honest: the marks
  // compare counts to each other and never to an absolute the viewer has not been given.
  const largest = data.reduce((m, d) => Math.max(m, d.count), 1);
  // **A small marker, not a disc over the cluster's ground.** A radius large enough to encode a
  // count covers hundreds of marks, and twenty of them overlap into a mess in the dense middle —
  // where the clusters are. Worse, a big circle reads as *extent*, which is exactly what this is
  // not: there is no geometry on the wire and the position is one point the publisher chose. A dot
  // with a number beside it claims only what it can.
  const radiusOf = (count: number) => 3 + 5 * Math.sqrt(count / largest);

  return [
    new ScatterplotLayer({
      id: 'artifact-rings',
      data,
      getPosition: (d: (typeof data)[number]) => d.position,
      getRadius: (d: (typeof data)[number]) => radiusOf(d.count),
      radiusUnits: 'pixels' as const,
      // **The panels' own colours, not a colour of its own.** An annotation is chrome over the
      // data, so it is drawn in the greys the rest of the instrument is drawn in — `#eaeef3` on
      // `#0d0f12`, the CSS's text and background. Giving it a hue would put it in competition with
      // the palette the marks are encoded by, and the one saturated colour this map has already
      // means something else: amber is the selected item.
      filled: true,
      getFillColor: [234, 238, 243, 240],
      stroked: true,
      getLineColor: [13, 15, 18, 235],
      lineWidthUnits: 'pixels' as const,
      getLineWidth: 1.5,
      // Comfortably clickable however small the count: the dot can be three pixels across, and a
      // cluster that cannot be hit is a cluster that cannot be opened.
      pickable: true,
      radiusMinPixels: 4,
      // Read by the click handler exactly as `tesseraIds` is for a mark — a different array under a
      // different name, because the two identify different kinds of thing and open different
      // routes. One endpoint answering both would let a caller learn which kind an identifier
      // names from the shape of the reply.
      artifactIds: data.map((d) => d.id),
      parameters: {depthCompare: 'always' as const},
      updateTriggers: {getRadius: largest}
    }),
    new TextLayer({
      id: 'artifact-counts',
      data,
      getPosition: (d: (typeof data)[number]) => d.position,
      getText: (d: (typeof data)[number]) => d.count.toLocaleString('en-GB'),
      getSize: 11,
      sizeUnits: 'pixels' as const,
      getColor: [234, 238, 243, 255],
      // The panels' typeface, so a count floating over the map is the same object as a count in a
      // panel rather than a second kind of thing.
      fontFamily: 'SFMono-Regular, Menlo, monospace',
      // Above its own dot, clear of it at every size — a label sitting *on* the marker is
      // unreadable over a dense patch however it is outlined.
      getPixelOffset: (d: (typeof data)[number]) => [0, -(radiusOf(d.count) + 9)],
      // A panel-coloured plate rather than a text outline: over a million coloured marks an
      // outline still leaves the glyphs competing with whatever is behind them. The plate is the
      // panels' own `rgba(13, 15, 18, 0.92)` over their own `#222` border, so the labels read as
      // the instrument's type floating over the map.
      background: true,
      getBackgroundColor: [13, 15, 18, 235],
      backgroundPadding: [4, 2, 4, 2],
      getBorderColor: [34, 34, 34, 255],
      getBorderWidth: 1,
      fontSettings: {sdf: true},
      fontWeight: 600,
      // Not pickable: the dot beneath it answers the click, and a label that swallowed picks would
      // move a cluster's hit area off the cluster.
      pickable: false,
      parameters: {depthCompare: 'always' as const},
      updateTriggers: {getPixelOffset: largest}
    })
  ];
}

/**
 * **Reusing the `data` object across paints was tried and reverted.** The slab hands back identical
 * typed arrays when nothing moved, so wrapping them in a stable object looked like the last step
 * needed to stop deck.gl re-uploading — but a fresh `ScatterplotLayer` instance carrying a `data`
 * object deck considers unchanged rendered large rectangles of the view black until something else
 * forced them to refill. Whatever deck does with layer state in that case, it is not what the
 * reasoning assumed, and the reasoning is not worth another try without knowing which.
 */
function selectionLayers(selectedWorldXY: [number, number] | null): Layer[] {
  const layers: Layer[] = [];

  if (selectedWorldXY) {
    layers.push(
      new ScatterplotLayer({
        id: 'selection',
        data: [selectedWorldXY],
        getPosition: (d: [number, number]) => d,
        getFillColor: [255, 210, 90, 255],
        radiusUnits: 'pixels' as const,
        getRadius: 5,
        stroked: true,
        getLineColor: [20, 20, 20, 255],
        lineWidthUnits: 'pixels' as const,
        getLineWidth: 1.5,
        parameters: {depthCompare: 'always' as const}
      })
    );
  }

  return layers;
}
