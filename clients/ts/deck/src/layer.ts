import {CompositeLayer, type BinaryAttribute as DeckBinaryAttribute, type CompositeLayerProps, type Layer, type LayersList, type UpdateParameters} from '@deck.gl/core';
import {BitmapLayer, LineLayer, PolygonLayer, ScatterplotLayer, TextLayer} from '@deck.gl/layers';
import {
  CLUSTER_PREFIX,
  NEUTRAL,
  NO_ORDINAL,
  gridToWorld,
  gridToWorldXY,
  type Artifact,
  type ArtifactsProjection,
  type LegendProjection,
  type MarksProjection,
  type Meta,
  type PresentedStatus,
  type Rgba,
  type Shape,
  type ShapeKind,
  type Store,
  type TilesProjection
} from '@tesseradb/client';
import {materialiseStandIn, type StandInBuffers} from './assemble.js';
import {buildColourAttribute, type Encoding} from './colour.js';
import {shapeBbox, smoothRing, type ContourShape, type Part} from './contours.js';
import {binDensity, filterDensity} from './density.js';
import {LABEL_LINE_HEIGHT, labelSize, placeLabels, wrapLabel, type LabelCandidate, type PlacedLabel} from './labels.js';
import {LookupTexture} from './lut.js';
import {MarksLayer} from './marks-layer.js';
import {deckOpacity, markStyle} from './marks-style.js';
import {MarkSlab, type GpuSlab} from './slab.js';

/**
 * `TesseraLayer` — a deck.gl `CompositeLayer` over the store's `marks`, `tiles` and `artifacts`
 * (design client-components §4, §5.10). What the instrument's layer construction and GPU slab
 * were, given a class boundary: for the customer who owns a `Deck` already, and for
 * `<tessera-map>`.
 *
 * **It never fetches.** Every projection arrives by property — or, with the `store` convenience,
 * is read off the store the host hands in, which the layer subscribes to and nothing more. Hover
 * and pick are the host's: the mark sublayers carry `tesseraIds` and the label sublayers
 * `artifactIds`, and `resolvePick` turns deck's pick info into a mark, an artifact, a miss or a
 * broken pick. **A contour is not in that pass**: what the pointer is over is resolved in JS
 * against the frontier's shapes ({@link contourShapes}, then `hoverAt`), for a hover and for a
 * click alike.
 *
 * **Every served mark is drawn.** The length handed to deck is the resident count, unconditionally
 * — no budget, no cap, no filter applies here. Colour is presentation and never decides what is
 * drawn: an unresolvable value is grey, never absent.
 *
 * **The slab is the host's**, because it outlives every frame and every response and is
 * GPU-facing storage rather than view state; the layer syncs it with the exact bands once per
 * `marks` object. The stand-in pieces are materialised once per `standIn` array, memoised on
 * its identity, since the pieces survive most frames by reference.
 *
 * The drawing, in order (§5.10): the hovered and the opened artifact's served `shape` — a hull,
 * a membership shape or an authored one, drawn through one path as parts with holes — or its
 * `box` where its layer draws no shape — **and nothing else**, since only those two draw
 * ({@link focusOutlines}); the single-hue
 * density wash from the exact tiles' counts,
 * filtered so the tile grid never shows (decision 0097); the marks — one `MarksLayer` per
 * retained slab partition, addressed by slot, plus the stand-ins — in their membership colour
 * through the lookup texture when colouring by cluster, else the column's colour; names and
 * counts at the frontier's centroids — sized by masked count — placed by priority into a spatial hash
 * with leader lines; the picked mark; and the selected region as the shape drawn — a box or a lasso, never
 * its cells.
 *
 * **Colour by cluster is exact only** (decision 0099): a point wears an artifact's colour only
 * because the wire named the point a member, through the ordinal it carries; the lookup texture
 * resolves the ordinal up the table to what is served now, and neutral where that fails. The
 * host owns the {@link LookupTexture} beside the slab; every colouring interaction — palette,
 * level, highlight, the cluster/column switch — is a rewrite of it or a uniform, never a pass
 * over the points (decision 0100).
 */

export type TesseraLayerProps = CompositeLayerProps & {
  /** A store to read every projection below from, for a host that would otherwise wire each. */
  store?: Store | null;
  marks?: MarksProjection | null;
  tiles?: TilesProjection | null;
  /** The depth the frame is drawn at — the slab's partition key and the wash's bin depth. */
  depth?: number;
  artifacts?: ArtifactsProjection | null;
  meta?: Meta | null;
  legend?: LegendProjection | null;
  status?: PresentedStatus;
  /** The persistent mark buffers, owned by the host. */
  slab: MarkSlab;
  /** The lookup texture, owned by the host beside the slab; made here when the host has none. */
  lut?: LookupTexture | null;
  /** The level to colour at for a nested layer; undefined colours at the deepest served. */
  clusterLevel?: number;
  /** Whether names and counts are drawn at the centroids. */
  labels?: boolean;
  /** The ground the map is drawn on; the ink, halo and outline weights follow it (the boards). */
  scheme?: 'light' | 'dark';
  /** The picked mark's world position, for its marker. */
  selectedWorldXY?: [number, number] | null;
  openedArtifact?: bigint | null;
  /** The artifact under the pointer — its outline or label, or a mark it holds. */
  hoveredArtifact?: bigint | null;
  /** The selected region's world shape, and the live shape while it is being drawn. */
  region?: [number, number, number, number] | null;
  regionPolygon?: [number, number][] | null;
  drag?: [number, number, number, number] | null;
  dragPolygon?: [number, number][] | null;
  /** Whether the density wash is drawn under the points. */
  wash?: boolean;
  /** A fixed mark radius in pixels; null sizes the marks by their count and the zoom (`markStyle`). */
  radius?: number | null;
  /** How many marks a paint ended up drawing, for the host's probe. */
  onDrawn?: ((drawn: number, provisional: number) => void) | null;
  /** Per-settle work, in ms — the slab sync, the wash bin, the lookup texture, the outlines, the labels, the whole layer build — and the mark style drawn, for the harness. */
  onTimings?: ((t: LayerTimings) => void) | null;
};

export type LayerTimings = {
  slabMs: number;
  washMs: number;
  lutMs: number;
  outlinesMs: number;
  labelsMs: number;
  layersMs: number;
  lutWrites: number;
  /**
   * The **parts** the outline layer holds and the **artifacts** they belong to; two units on
   * purpose, because a shape is a list of parts. Both count what draws — the hovered and the
   * opened artifact, so `outlinesDrawn` is 0, 1 or 2 — and neither counts what may be hovered,
   * which is the frontier and is no longer the layer's to hold ({@link contourShapes}). The two
   * differ where the drawn artifact is several pieces: two parts, one shape.
   */
  outlines: number;
  outlinesDrawn: number;
  labels: number;
  /** The mark style the paint drew: radius in pixels and composited alpha, from `markStyle`. */
  markRadius: number;
  markAlpha: number;
  /** The resident count the style was chosen for. */
  markCount: number;
};

/** How zoom is bucketed for label placement: a quarter of a zoom level. */
const LABEL_ZOOM_STEP = 4;

/** A layer's colouring, as the store's `colourBy` names it: `cluster:<layer>` or a column. */
export function clusterLayerOf(colourBy: string | null | undefined): string | null {
  return colourBy && colourBy.startsWith(CLUSTER_PREFIX) ? colourBy.slice(CLUSTER_PREFIX.length) : null;
}

type Resolved = {
  marks: MarksProjection | null;
  tiles: TilesProjection | null;
  depth: number;
  artifacts: ArtifactsProjection | null;
  meta: Meta | null;
  legend: LegendProjection | null;
  status: PresentedStatus;
};

/** The selection's colour: the accent, as the boards draw the lasso (`gen.py`'s `lasso_layer`). */
const ACCENT: Record<'light' | 'dark', [number, number, number]> = {light: [36, 87, 163], dark: [134, 176, 240]};
/** Label ink and its halo per ground (`datamap_layers2`). */
const INK: Record<'light' | 'dark', [number, number, number]> = {light: [36, 39, 43], dark: [236, 238, 240]};
/** The halo's own colour: the boards' 0.85 on both grounds (`gen.py`'s `datamap_layers`). */
const HALO: Record<'light' | 'dark', [number, number, number, number]> = {light: [247, 247, 244, 217], dark: [12, 14, 17, 217]};
/**
 * The halo's width as a fraction of the em, following the boards' 0.32 em stroke painted under
 * the fill — half of which shows outside the glyph, so about 0.16 em of outline: 1.9 px on a
 * 12 px name and 3.5 px on a 22 px one.
 */
const HALO_EM = 0.16;
const CHROME: [number, number, number, number] = [234, 238, 243, 240];
const PLATE: [number, number, number, number] = [13, 15, 18, 235];

/**
 * The current colour encoding, from the store's legend and the schema.
 *
 * Falls back to uniform rather than throwing at every step where the state is not yet ready — a
 * column chosen before its values resolved, a refused `/v1/categories`. Colour is presentation,
 * so an incomplete encoding degrades to a drawn map, never to no map. A refused column colours
 * every mark *unmapped*, not uniform: uniform means no encoding chosen, unmapped means this value
 * could not be named, and the legend says the latter.
 */
export function encodingOf(meta: Meta | null, legend: LegendProjection | null): Encoding {
  const colourBy = legend?.colourBy ?? null;
  if (!colourBy || !meta || !legend || colourBy.startsWith(CLUSTER_PREFIX)) return {kind: 'uniform'};
  const column = meta.declaredScalars.find((c) => c.name === colourBy);
  if (!column) return {kind: 'uniform'};
  if (legend.categoryErrors[colourBy]) return {kind: 'unmapped'};
  if (column.category) {
    // Paint follows the ranks, and the ranks are local: assigned from the codes counted in held
    // bands, no round trip. The map is painted the moment the count lands and does not wait for
    // `/v1/categories`, which names a colour in the legend and nothing else.
    const rankOfCode = legend.ranks[colourBy];
    if (!rankOfCode || Object.keys(rankOfCode).length === 0) return {kind: 'uniform'};
    return {kind: 'category', column: colourBy, rankOfCode};
  }
  const domain = legend.domains[colourBy];
  if (!domain) return {kind: 'uniform'};
  return {kind: 'numeric', column: colourBy, domain};
}

/**
 * What the current colouring *is*, as a string — the paint key's colour half. Sizes rather than
 * contents, because ranks and domains are sticky accumulators that only ever grow.
 */
export function encodingSignature(encoding: Encoding): string {
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

/**
 * A binary attribute descriptor, reused for as long as its buffer is the same object.
 *
 * deck.gl's skip check is reference equality on this descriptor, not on the array inside it, so
 * a fresh literal each paint re-uploads an attribute whose bytes have not changed — measured in the
 * instrument at 32.9 MB uploaded per pan where 16.4 MB was needed. Keyed on the array, so a
 * republished buffer always uploads and an unchanged one never does.
 */
type BinaryAttribute<T extends ArrayBufferView> = {value: T; size: number; normalized?: boolean};
const descriptors = new WeakMap<ArrayBufferView, BinaryAttribute<ArrayBufferView>>();

function binary<T extends ArrayBufferView>(value: T, size: number, normalized?: boolean): BinaryAttribute<T> {
  let held = descriptors.get(value);
  if (!held) {
    held = normalized === undefined ? {value, size} : {value, size, normalized};
    descriptors.set(value, held);
  }
  return held as BinaryAttribute<T>;
}

/**
 * Attribute descriptors around a partition's own GPU buffers, keyed by attribute name so deck
 * binds the buffer (`setExternalBuffer`) rather than copying it. Memoised on the `GpuSlab`,
 * which the partition keeps stable across appends.
 */
type AttributeMap = Record<string, DeckBinaryAttribute>;
const gpuDescriptors = new WeakMap<GpuSlab, AttributeMap>();

function gpuAttributes(gpu: GpuSlab): AttributeMap {
  let held = gpuDescriptors.get(gpu);
  if (!held) {
    held = {
      instancePositions: {buffer: gpu.positions, size: 2, type: 'float32', stride: 8, offset: 0},
      instanceFillColors: {buffer: gpu.colours, size: 4, type: 'unorm8', stride: 4, offset: 0},
      instancePickingColors: {buffer: gpu.picking, size: 4, type: 'uint8', stride: 4, offset: 0},
      instanceOrdinals: {buffer: gpu.ordinals, size: 1, type: 'float32', stride: 4, offset: 0}
    };
    gpuDescriptors.set(gpu, held);
  }
  return held;
}

/** The stand-in buffers, once per piece list — the pieces survive most frames by reference. */
const heldStandIn = new WeakMap<object, {key: string; buffers: StandInBuffers}>();
/** The stand-in colours, once per (buffers, encoding). */
const heldStandInColours = new WeakMap<object, {key: string; colours: Uint8Array}>();
/** Frames whose slab-residency check has run — once per `marks` object, not once per paint. */
const checkedMarks = new WeakSet<object>();
/** The wash image, once per `tiles` object; `pending` while it is being built off the paint path. */
type HeldWash = {depth: number; image: ImageData | null; bounds: [number, number, number, number]; pending: boolean};
const heldWash = new WeakMap<object, HeldWash>();
/** The last wash built, drawn while the next is being built. */
let lastWash: HeldWash | null = null;
/** The pending wash build, one at a time: a newer `tiles` object supersedes an unbuilt older one. */
let washTimer: ReturnType<typeof setTimeout> | null = null;
/** How long `tiles` must stand still before the wash is rebuilt — the settle, not the frame. */
const WASH_SETTLE_MS = 200;
/** What the empty sublayers are given, once, so their descriptors are stable across paints. */
const EMPTY_F32 = new Float32Array(0);
const EMPTY_U8 = new Uint8Array(0);
const EMPTY_IDS = new BigUint64Array(0);
const EMPTY_IMAGE = typeof ImageData !== 'undefined' ? new ImageData(1, 1) : null;
const NO_OUTLINES: OutlineDatum[] = [];
const NO_LABELS: LabelDatum[] = [];
const NO_LEADERS: LeaderDatum[] = [];
const NO_SHAPES: {polygon: [number, number][]}[] = [];
const NO_POINTS: [number, number][] = [];
/** The column encoding the slab's colour attribute holds, kept while the map colours by cluster. */
const heldColumnEncoding = new WeakMap<MarkSlab, {encoding: Encoding; colourBy: string | null}>();
/** A lookup texture per slab, for a host that handed none in. */
const ownLut = new WeakMap<MarkSlab, LookupTexture>();
/** The drawn outlines, once per served set, fetched shapes, opened artifact and hovered artifact. */
const heldOutlines = new WeakMap<object, {key: string; shapes: object; data: OutlineDatum[]}>();
/** The label placement, once per served set and zoom bucket. */
const heldLabels = new WeakMap<object, {key: string; data: LabelDatum[]; leaders: LeaderDatum[]; placed: number}>();
/**
 * The label candidates, once per served set — **not** per zoom bucket, which is what the
 * placement is per. Anchors are held in world units and scaled per bucket; see
 * {@link labelCandidates}.
 */
const heldCandidates = new WeakMap<object, {key: string; candidates: LabelCandidate[]; byId: Map<bigint, LabelText>}>();

/**
 * One drawn part — its outer ring and its holes, the nesting deck's `PolygonLayer` takes. **A
 * datum is a part, not an artifact** — a shape is a list of parts (`polygon-membership.md` §7.1),
 * and every part of one artifact carries that artifact's `id`, so an artifact whose members are
 * two separated clouds is two rows here that draw alike.
 */
export type OutlineDatum = {
  id: bigint;
  polygon: [number, number][][];
  colour: Rgba;
  opened: boolean;
  hovered: boolean;
  /** The wire's `rung` — the resolution the artifact is drawn at (contracts §3.2 r44). */
  rung: number;
  /** Which shape the wire answered with, and so whether the ring was smoothed ({@link outlineOf}). */
  source: OutlineSource;
  /** The fill and line alphas (0–255) and the line width in pixels this outline draws with. */
  fill: number;
  line: number;
  width: number;
};
type LabelDatum = {
  id: bigint;
  position: [number, number];
  text: string;
  size: number;
  offset: [number, number];
  colour: Rgba;
  kind: 'name' | 'count' | 'topic';
  /** Where the offset sits on the run: a wrapped name centres, the last line ends at the seam. */
  anchor: 'start' | 'middle' | 'end';
};
type LeaderDatum = {from: [number, number]; to: [number, number]};

/** The hovered outline: a faint fill and a firm line, less than the opened one's. */
const HOVER_FILL: Record<'light' | 'dark', number> = {light: 26, dark: 33};
const HOVER_LINE = 150;
/** The opened outline: the boards' 0.16 fill and a strong line. */
const OPENED_FILL = 41;
const OPENED_LINE = 200;
/**
 * A `box` draws as an unfilled hairline rectangle, whichever of the two states it is in.
 *
 * A box is the axis-aligned bounds of the visible members, not their shape: tinting its interior
 * would wash ground the members need not occupy at all, and a heavy line would draw the eye to a
 * rectangle that is a summary of an extent rather than a boundary anyone dug. The line still
 * carries the artifact's own colour and the state's own alpha, so the highlight reads.
 */
const BOX_LINE_WIDTH = 0.8;

export type OutlineOptions = {
  opened: bigint | null;
  hovered: bigint | null;
  level: number | undefined;
  scheme: 'light' | 'dark';
  /** The layer roster, to leave a dependent layer's artifacts out; null draws every served layer. */
  meta?: Meta | null;
};

/** What is on the map at all: the level cut and the layer roster, and nothing about what draws. */
export type ContourOptions = {
  level: number | undefined;
  meta?: Meta | null;
};

/**
 * Whether a served artifact is drawn at `level` and has no drawn child — {@link frontier}'s rule,
 * asked of one artifact rather than of the whole served set, which is what the hovered and the
 * opened artifact need.
 */
function onFrontier(a: ArtifactsProjection, artifact: Artifact, level: number | undefined): boolean {
  const drawn = (x: {rung: number}) => level === undefined || x.rung <= level;
  if (!drawn(artifact)) return false;
  return !(a.lineage.childrenOf.get(artifact.tesseraId) ?? []).some((c) => drawn(c));
}

/** The layers whose artifacts attach their text to another layer's, and draw no shape of their own. */
function dependentLayers(meta: Meta | null | undefined): Set<string> {
  return new Set(meta?.layers.filter((l) => l.depsOn.length > 0).map((l) => l.name) ?? []);
}

/**
 * Which kind of shape a layer draws — `/v1/meta`'s `shape` (`polygon-membership.md` §7.1), the
 * layer's own declaration and not a property of any one artifact. With no roster in hand this is
 * null, and the box draws.
 */
function shapeKindOf(meta: Meta | null | undefined, layer: string): ShapeKind | null {
  return meta?.layers.find((l) => l.name === layer)?.shape ?? null;
}

/**
 * **The shapes a viewer may point at**: one per served artifact on the **frontier**, carrying the
 * wire's own vertices — the served ring, never the drawn curve — for {@link hoverAt} to resolve a
 * hover or a click against.
 *
 * **The frontier and nothing above it** (the owner's review, 2026-08-27). A response carries a
 * frontier *and its ancestors*, and every served artifact used to sit in a polygon layer at zero
 * alpha so that it still answered deck's pick — which made an ancestor nobody can see reachable by
 * pointing at it: the pointer crossed a parent's invisible ring on its way across a child's and
 * the hover flipped between the two. A shape that is never drawn is not a thing a viewer can point
 * at, so it is not here. This is the same set the labels are drawn from — what carries a name
 * carries a contour and answers a hover. A dependent layer's artifacts (a clustering's topic
 * labels) are out for the same reason: their text is drawn beneath the name of the artifact they
 * attach to and they have no shape of their own, so their `box` fallback would put a rectangle
 * over the map with nothing drawn on it.
 *
 * **A shape is the artifact's `box` until its served shape arrives.** The viewport is asked for
 * centroids and boxes, so at rest every entry here is a rectangle; the shape for whatever the
 * pointer lands on is fetched by identifier (`TesseraStore.needShape`) and this is rebuilt
 * around it when it lands. A layer that draws no shape draws its box and this is the shape for
 * good.
 *
 * A shape's parts are separate pieces — a hull's α-groups (`artifact-shapes.md` §1), a
 * boundary's exclaves — so one entry holds several parts and a pointer between two of them is
 * inside neither, and a pointer in a hole is outside.
 */
export function contourShapes(a: ArtifactsProjection, o: ContourOptions): ContourShape[] {
  const dependent = dependentLayers(o.meta);
  const front = frontier(a, o.level);
  const shapes: ContourShape[] = [];
  for (const artifact of a.served) {
    if (!front.has(artifact.tesseraId)) continue;
    if (dependent.has(artifact.layer)) continue;
    const outline = outlineOf(artifact, a.shapes?.get(artifact.tesseraId));
    if (!outline) continue;
    shapes.push({id: artifact.tesseraId, rung: artifact.rung, parts: outline.parts, bbox: shapeBbox(outline.parts)});
  }
  return shapes;
}

/**
 * **What draws: the hovered artifact and the opened one, and nothing else** — on every layer,
 * nested included (the owner's review, 2026-08-26). At rest the map is colour and names.
 *
 * The rule already held for a flat layer, whose hulls overlap into a mesh, and the argument is the
 * same one level up: a response carries a frontier and its ancestors, so drawing every served hull
 * gave each region its own shape plus its parent's plus its grandparent's, translucent fills
 * stacking into a murky wash with no cue that the big shape contained the small ones. Exact colour
 * already says where a cluster is and how far it reaches, per principal, so the contours were
 * paying for a thing already drawn.
 *
 * **So this is asked about two artifacts and never about the served set.** The rest of the
 * frontier used to be handed to deck at zero alpha to answer a pick, which made a hover change
 * cost a pass over every served artifact and a re-tessellation of every frontier ring: measured at
 * 38 ms building the rows and 16 ms more in deck's tessellator, at the 34,385 served boxes an
 * administrative hierarchy puts on screen at zoom 9, for every boundary the pointer crossed. This
 * is 0.01 ms at that count, and the pick those rows answered is resolved against
 * {@link contourShapes} instead — 17 ms once per served set, then 0.75 ms per pointer move.
 *
 * **A box is not smoothed, and where a shape is coming it is not drawn at all.** {@link smoothRing}
 * is a periodic cubic B-spline, and four corners through it is an oval: the rectangle a viewer
 * hovers must be the rectangle the wire sent. Where the layer draws a shape the box is a
 * placeholder for one that is on its way by identifier, so nothing draws until it lands —
 * a rectangle that becomes the shape a moment later reads as the shape changing under the
 * pointer. The hover still resolves against the box meanwhile ({@link contourShapes}), which is
 * what asks for the shape in the first place.
 *
 * **Only a derived shape is smoothed.** A hull's vertices are member positions and the spline is
 * the summary `artifact-shapes.md` §4 argues for; a predicate or an authored shape is a boundary
 * somebody drew, already generalised to the pixel by the server's vertex rule, and a curve
 * through its vertices would move a border and could cross its own holes. Both draw in the same
 * style — the opened artifact strong with a faint fill, the hovered one lighter.
 *
 * **A served artifact contributes one row per part of its shape**, so the length of the result is
 * the part count and not the artifact count. Every row of one artifact draws alike, because the
 * parts are one shape in several pieces and highlighting half of a cluster would be a lie about
 * where its members are. Parents are ordered first so an opened child sits over an opened parent.
 */
export function focusOutlines(a: ArtifactsProjection, o: OutlineOptions): OutlineDatum[] {
  const dependent = dependentLayers(o.meta);
  const data: OutlineDatum[] = [];
  const seen = new Set<bigint>();
  // Opened first, so that opening the artifact under the pointer draws it opened and not hovered.
  for (const id of [o.opened, o.hovered]) {
    if (id === null || seen.has(id)) continue;
    seen.add(id);
    const artifact = a.served.find((x) => x.tesseraId === id);
    if (!artifact) continue;
    if (dependent.has(artifact.layer)) continue;
    if (!onFrontier(a, artifact, o.level)) continue;
    const outline = outlineOf(artifact, a.shapes?.get(id));
    if (!outline) continue;
    const box = outline.source === 'box';
    const kind = shapeKindOf(o.meta, artifact.layer);
    if (box && kind !== null) continue;
    // With no roster in hand a served shape is smoothed as a hull always was.
    const smooth = !box && (kind === null || kind === 'derived');
    const opened = id === o.opened;
    const ordinal = a.table.ordinalOf(artifact.layer, artifact.tesseraId);
    const colour = a.colours.get(ordinal) ?? NEUTRAL;
    const fill = box ? 0 : opened ? OPENED_FILL : HOVER_FILL[o.scheme];
    const line = opened ? OPENED_LINE : HOVER_LINE;
    const width = box ? BOX_LINE_WIDTH : opened ? 1.2 : 1;
    for (const part of outline.parts) {
      data.push({
        id: artifact.tesseraId,
        polygon: smooth ? part.map((ring) => smoothRing(ring)) : part.map((ring) => [...ring]),
        colour,
        opened,
        hovered: !opened,
        rung: artifact.rung,
        source: outline.source,
        fill,
        line,
        width
      });
    }
  }
  // The wire's `rung` orders parents first (contracts §3.2 r44): the declared level on a levelled
  // layer, the response-local chain depth on a treed one — drawn by, never derived.
  return data.sort((x, y) => x.rung - y.rung);
}

/**
 * How many labels a viewport of `width` × `height` pixels is given: the top N by masked count
 * are placed and the rest wait for a zoom (§5.10). One per 36,000 px² — twenty-eight on a
 * 1280 × 800 viewport — and never fewer than eight.
 */
export function labelBudget(width: number, height: number): number {
  return Math.max(8, Math.floor((width * height) / 36_000));
}

export type LabelText = {
  artifact: Artifact;
  /** The name as it is drawn: up to three short lines (`wrapLabel`). */
  lines: string[];
  countText: string;
  size: number;
  topic: string | null;
};

/** The width of a run of text at a font size, in pixels — the model the placement box uses. */
const NAME_EM = 0.58;
const COUNT_EM = 0.55;
/** The count's size relative to the name's, and the gap between the last line and it. */
const COUNT_SCALE = 0.82;
const COUNT_GAP_EM = 0.35;
/** The topic line beneath the block, in pixels. */
const TOPIC_SIZE = 12;

/**
 * The **frontier** of the served set at `level`: every drawn artifact with no drawn child.
 *
 * A served artifact that has a served child in the same response is an ancestor of something on
 * the map. It names nothing its children do not name more precisely, so it draws no label (the
 * owner's review, 2026-08-26) — computed here from `parentIds` over the served set, which is what
 * `lineage` already holds, so nothing new is asked of the wire.
 *
 * With no chosen level this is the cut's leaves. With one it is that level's artifacts **and**
 * every shallower artifact whose own children the level cut away, which the plain `rung === level`
 * test that stood here dropped — a branch that stops above the chosen level went unnamed.
 *
 * The number compared is the wire's `rung` (contracts §3.2 r44): the declared level on a levelled
 * layer, the response-local chain depth on a treed one, computed server-side after the cut. It is
 * never derived here — the client-side chain count this once used answered the wrong question on
 * a levelled layer, whose edges may skip a level (trap 5.4, retired with the column).
 */
export function frontier(a: ArtifactsProjection, level: number | undefined): Set<bigint> {
  const out = new Set<bigint>();
  for (const artifact of a.served) if (onFrontier(a, artifact, level)) out.add(artifact.tesseraId);
  return out;
}

/**
 * The label candidates for a served set at `zoom`: the frontier's artifacts **that have a text to
 * draw** — an artifact with no supplied text and no attached topic draws no label, never its key,
 * which is an id — the top `budget` of them by masked count, each with its name, count and topic
 * and the pixel box the placement needs.
 *
 * **A name's size is its masked count's**, on {@link labelSize}'s logarithmic band over the range
 * the drawn frontier holds. The range is taken over the candidates that survive the budget, which
 * is what is on screen: the largest name is the largest count drawn, and the smallest the
 * smallest.
 *
 * **Held per served set, not per zoom bucket.** Every part of a candidate but its anchor — the
 * frontier, the sort by masked count, the budget's cut, the wrapped lines and the box they make —
 * is a function of the served set, the level and the budget alone; only the anchor is pixels, and
 * a zoom scales it. The placement is what a bucket re-runs, and it takes the top `budget`
 * candidates rather than the served set (0.2 ms against 38 for a filter and sort of 34k, measured
 * on GeoNames at zoom 9–10). So the list is built once and each bucket scales anchors into a copy.
 */
export function labelCandidates(a: ArtifactsProjection, meta: Meta | null, level: number | undefined, zoom: number, budget: number): {candidates: LabelCandidate[]; byId: Map<bigint, LabelText>} {
  const key = `${a.version}|${level ?? ''}|${budget}`;
  let held = heldCandidates.get(a.served);
  if (!held || held.key !== key) {
    held = {key, ...namedCandidates(a, meta, level, budget)};
    heldCandidates.set(a.served, held);
  }
  const scale = 2 ** zoom; // pixels per world unit
  return {candidates: held.candidates.map((c) => ({...c, x: c.x * scale, y: c.y * scale})), byId: held.byId};
}

/**
 * The zoom-independent half of {@link labelCandidates}: the candidates with their anchors in
 * **world units**, which the caller scales.
 */
function namedCandidates(a: ArtifactsProjection, meta: Meta | null, level: number | undefined, budget: number): {candidates: LabelCandidate[]; byId: Map<bigint, LabelText>} {
  const placed = a.served.filter((x) => x.centroid !== null);
  // A dependent layer's artifacts — a clustering's topic labels — draw their text beneath the
  // name of whatever they sit on, italic and small, and are placed with it: they are not
  // candidates of their own (§5.10, D13).
  const dependent = new Set(meta?.layers.filter((l) => l.depsOn.length > 0).map((l) => l.name) ?? []);
  const topicOf = attachedTopics(a, meta);
  const front = frontier(a, level);
  const named = placed
    .filter((x) => !dependent.has(x.layer) && front.has(x.tesseraId))
    .filter((x) => hasText(x) || topicOf.has(x.tesseraId))
    .sort((x, y) => Number(y.maskedCount - x.maskedCount))
    .slice(0, Math.max(0, budget));
  // The range the band is drawn over: the counts of the names that will actually be on screen.
  let smallest = Number.POSITIVE_INFINITY;
  let largest = 0;
  for (const x of named) {
    const count = Number(x.maskedCount);
    if (count < smallest) smallest = count;
    if (count > largest) largest = count;
  }
  const candidates: LabelCandidate[] = [];
  const byId = new Map<bigint, LabelText>();
  for (const artifact of named) {
    const count = Number(artifact.maskedCount);
    const size = labelSize(count, smallest, largest);
    const attached = topicOf.get(artifact.tesseraId) ?? null;
    // A cluster with no name of its own takes its topic as the name (a labelled clustering);
    // one with both draws the topic beneath in italic (the boards).
    const name = artifactName(artifact) ?? attached!;
    const topic = artifactName(artifact) === null ? null : attached;
    const countText = count.toLocaleString('en-GB');
    // **The wrapped box is what is placed.** A name is drawn as up to three short lines, so the
    // spatial hash packs against the block the viewer sees and the 40 px displacement rule is
    // measured against it — a one-line box three hundred pixels wide overlapped everything.
    const lines = wrapLabel(name);
    byId.set(artifact.tesseraId, {artifact, lines, countText, size, topic});
    const widest = lines.reduce((w, line) => Math.max(w, line.length * NAME_EM * size), 0);
    const lastLine = lines[lines.length - 1]!.length * NAME_EM * size + countText.length * COUNT_EM * size * COUNT_SCALE + COUNT_GAP_EM * size;
    candidates.push({
      id: artifact.tesseraId,
      x: gridToWorld(artifact.centroid![0]),
      y: gridToWorld(artifact.centroid![1]),
      width: Math.max(widest, lastLine, topic ? topic.length * TOPIC_SIZE * 0.5 : 0) + 8,
      height: lines.length * size * LABEL_LINE_HEIGHT + (topic ? TOPIC_SIZE + 3 : 0),
      priority: count
    });
  }
  return {candidates, byId};
}

/** Whether an artifact carries a text to draw: its first supplied content, non-empty. */
export function hasText(a: Artifact): boolean {
  return (a.content[0] ?? '').length > 0;
}

/**
 * What to call an artifact: its supplied text where the layer publishes any, else **nothing**.
 *
 * A key is an identifier its layer's author chose — `hdb-2422486`, `tp2-000002` — and drawn as a
 * name it reads as a cluster called that (the owner's review, 2026-08-26). The map draws no label
 * for an artifact with no text; a panel with a row to fill draws a neutral placeholder beside the
 * count, and shows the key under the field that says *key*.
 */
export function artifactName(a: Artifact): string | null {
  const text = a.content[0];
  return text !== undefined && text.length > 0 ? text : null;
}

/** Which of the two the wire answered an artifact's outline with: its served shape, or its box. */
export type OutlineSource = 'shape' | 'box';

/** A served artifact's outline in world space — parts of rings — and which of the two it is. */
export type Outline = {parts: Part[]; source: OutlineSource};

/**
 * A served artifact's outline in world space: **its served shape's parts**, else its box, else
 * nothing — the wire's own vertices, in the wire's own order, and nothing else.
 *
 * **It says which of the two it returned**, because the two are drawn differently and a caller
 * cannot tell them apart by counting vertices: a box is four corners and so is a square shape.
 * A derived shape is smoothed and a box never is ({@link focusOutlines}), and four corners
 * through a periodic cubic B-spline is an oval.
 *
 * A shape is parts of rings (`polygon-membership.md` §7.1) — a hull's α-groups one part each, a
 * boundary's exclaves as parts and its enclaves as holes — so this returns a list of parts and
 * each one is drawn as its own polygon with holes carrying the artifact's identifier. A ring of
 * one or two vertices is degenerate — a hull's group of one or two members, with no area to draw
 * or to pick — and is left out; a part whose outer ring is degenerate goes whole, holes and all,
 * since a surviving hole drawn first would be the polygon; where that leaves no part at all the
 * box answers instead, which is the rule a degenerate single hull already met.
 *
 * **`fetched` is the shape the drill-down route answered with**, and it wins over the artifact's
 * own where both exist. The viewport is asked for centroids and boxes, so a served row carries no
 * shape and the shape for the one artifact that draws arrives by identifier
 * (`TesseraStore.needShape`); until it does, the box is what is drawn, which is the same fallback
 * a layer drawing no shape has always taken.
 *
 * **This returns the wire's vertices and does not smooth them.** The smoothing is
 * {@link smoothRing}, applied by {@link focusOutlines} to a derived shape's rings. It is a
 * **periodic cubic B-spline** through the served ring rather than a containment-preserving corner
 * cut: the curve may sit a little outside the served ring at a reflex corner, bounded by a sixth
 * of the second difference there, which is a less precise summary of where the cluster is and not
 * a claim about ground the members do not occupy (`artifact-shapes.md` §4, the owner's ruling of
 * 2026-08-28). Every vertex the engine sends is still a visible member's own position, and the
 * served ring — not the drawn curve — is what a pick reads. **Neither is a membership test**: a
 * served shape is a drawing, and whether a point belongs to the artifact is the wire's
 * `membership:<layer>` column's answer (`polygon-membership.md` §7.1).
 */
export function outlineOf(a: Artifact, fetched?: Shape | null): Outline | null {
  const w = gridToWorld;
  const parts: Part[] = [];
  for (const rings of fetched ?? a.shape ?? []) {
    const outer = rings[0];
    if (!outer || outer.length < 3) continue;
    parts.push(rings.filter((ring) => ring.length >= 3).map((ring) => ring.map(gridToWorldXY)));
  }
  if (parts.length > 0) return {parts, source: 'shape'};
  if (a.box) {
    return {parts: [[[[w(a.box[0]), w(a.box[1])], [w(a.box[2]), w(a.box[1])], [w(a.box[2]), w(a.box[3])], [w(a.box[0]), w(a.box[3])]]]], source: 'box'};
  }
  return null;
}

/**
 * The text a dependent layer's artifacts (a clustering's topic labels) attach to the served
 * artifacts of their generating layer. A dependent artifact carries its target's masked count
 * and no id (D13), and no centroid on this wire: it is attached to the served artifact of its
 * generating layer with the same count, and left unattached where two share one — a count is
 * not an identity. A target id on the wire (S4's drill-down route, awaiting a ruling) would make
 * this exact.
 */
export function attachedTopics(a: ArtifactsProjection, meta: Meta | null): Map<bigint, string> {
  const topicOf = new Map<bigint, string>();
  if (!meta) return topicOf;
  const dependent = new Set(meta.layers.filter((l) => l.depsOn.length > 0).map((l) => l.name));
  const byCount = new Map<string, Artifact[]>();
  for (const x of a.served) {
    if (dependent.has(x.layer)) continue;
    const k = `${x.layer}|${x.maskedCount}`;
    (byCount.get(k) ?? byCount.set(k, []).get(k)!).push(x);
  }
  for (const t of a.served) {
    if (!dependent.has(t.layer) || t.content.length === 0) continue;
    const generating = meta.layers.find((l) => l.name === t.layer)?.depsOn ?? [];
    for (const g of generating) {
      const targets = byCount.get(`${g}|${t.maskedCount}`);
      if (targets && targets.length === 1) topicOf.set(targets[0]!.tesseraId, t.content[0]!);
    }
  }
  return topicOf;
}

/**
 * What to call a served artifact: its own text, else a topic attached to it, else **nothing** —
 * a caller with a row to fill draws a neutral placeholder rather than the key ({@link artifactName}).
 */
export function displayName(artifact: Artifact, topics: ReadonlyMap<bigint, string>): string | null {
  return artifactName(artifact) ?? topics.get(artifact.tesseraId) ?? null;
}

export class TesseraLayer extends CompositeLayer<TesseraLayerProps> {
  static override layerName = 'TesseraLayer';
  static override defaultProps = {
    store: null,
    marks: null,
    tiles: null,
    depth: 0,
    artifacts: null,
    meta: null,
    legend: null,
    status: 'idle',
    lut: null,
    clusterLevel: undefined,
    labels: true,
    scheme: 'dark',
    selectedWorldXY: null,
    openedArtifact: null,
    region: null,
    regionPolygon: null,
    drag: null,
    dragPolygon: null,
    hoveredArtifact: null,
    wash: true,
    radius: null,
    pickable: true,
    onDrawn: null,
    onTimings: null
  };

  declare state: {tick: number; unsubscribe: (() => void) | null; subscribed: Store | null; zoomBucket: number};

  override initializeState(): void {
    this.state = {tick: 0, unsubscribe: null, subscribed: null, zoomBucket: NaN};
    this.follow(this.props.store ?? null);
  }

  /**
   * Labels are placed in screen space, so a zoom re-places them; a pan does not (the placement
   * is translation-invariant). The layer rebuilds on a viewport change only when the zoom
   * crosses a bucket, which keeps a drag from rebuilding every sublayer per frame.
   */
  override shouldUpdateState(params: UpdateParameters<this>): boolean {
    if (super.shouldUpdateState(params)) return true;
    if (!params.changeFlags.viewportChanged) return false;
    const bucket = Math.round((params.context.viewport?.zoom ?? 0) * LABEL_ZOOM_STEP);
    return bucket !== this.state.zoomBucket;
  }

  override updateState(params: UpdateParameters<this>): void {
    super.updateState(params);
    if (params.changeFlags.propsChanged && (this.props.store ?? null) !== this.state.subscribed) {
      this.follow(this.props.store ?? null);
    }
  }

  override finalizeState(context: Parameters<CompositeLayer['finalizeState']>[0]): void {
    super.finalizeState(context);
    this.state.unsubscribe?.();
    this.state.unsubscribe = null;
    this.state.subscribed = null;
  }

  /** The `store` convenience: subscribe, and mark the layer for update on every change. */
  private follow(store: Store | null): void {
    this.state.unsubscribe?.();
    this.state.unsubscribe = null;
    this.state.subscribed = store;
    if (!store) return;
    this.state.unsubscribe = store.subscribe(() => {
      this.setState({tick: this.state.tick + 1});
    });
  }

  private resolved(): Resolved {
    const p = this.props;
    const s = p.store;
    if (s) {
      const view = s.get('view');
      return {
        marks: p.marks ?? s.get('marks'),
        tiles: p.tiles ?? s.get('tiles'),
        depth: p.depth || view.depth,
        artifacts: p.artifacts ?? s.get('artifacts'),
        meta: p.meta ?? s.get('meta'),
        legend: p.legend ?? s.get('legend'),
        status: p.status && p.status !== 'idle' ? p.status : s.get('status').status
      };
    }
    return {
      marks: p.marks ?? null,
      tiles: p.tiles ?? null,
      depth: p.depth ?? 0,
      artifacts: p.artifacts ?? null,
      meta: p.meta ?? null,
      legend: p.legend ?? null,
      status: p.status ?? 'idle'
    };
  }

  /** The lookup texture: the host's, or one kept per slab. */
  private lut(): LookupTexture {
    if (this.props.lut) return this.props.lut;
    let held = ownLut.get(this.props.slab);
    if (!held) {
      held = new LookupTexture();
      ownLut.set(this.props.slab, held);
    }
    return held;
  }

  override renderLayers(): LayersList {
    const started = performance.now();
    const timings: LayerTimings = {slabMs: 0, washMs: 0, lutMs: 0, outlinesMs: 0, labelsMs: 0, layersMs: 0, lutWrites: 0, outlines: 0, outlinesDrawn: 0, labels: 0, markRadius: 0, markAlpha: 0, markCount: 0};
    this.state.zoomBucket = Math.round((this.context.viewport?.zoom ?? 0) * LABEL_ZOOM_STEP);
    const layers = this.buildLayers(timings);
    timings.layersMs = performance.now() - started;
    timings.lutWrites = this.lut().writes;
    this.props.onTimings?.(timings);
    return layers;
  }

  private buildLayers(timings: LayerTimings): LayersList {
    const r = this.resolved();
    const {slab} = this.props;
    const layers: (Layer | null)[] = [];

    // The lookup texture is rewritten whenever the table, the colours, the palette, the level or
    // the highlight moved — never O(points), and by the rows that moved rather than whole where
    // the table only gained ordinals (`lut.ts`) — and the device it lives on is deck's. **The
    // table and the colour map are compared inside**, by version and by identity: a point response
    // names artifacts the debounced channel has not served yet, and those ordinals would otherwise
    // keep the texture's neutral until the channel's next answer. The served set's version is
    // deliberately not in the key — no texel is a function of it, and having it there rebuilt the
    // whole texture on every settle.
    const lut = this.lut();
    const lutStarted = performance.now();
    if (this.context.device && !lut.gpu) lut.attach(this.context.device);
    const opened = this.props.openedArtifact ?? null;
    const highlight = r.artifacts && opened !== null ? r.artifacts.served.find((a) => a.tesseraId === opened) : undefined;
    const highlightOrdinal = highlight && r.artifacts ? r.artifacts.table.ordinalOf(highlight.layer, highlight.tesseraId) : NO_ORDINAL;
    if (r.artifacts) {
      lut.update(
        {artifacts: r.artifacts, level: this.props.clusterLevel, highlight: highlightOrdinal},
        `${r.artifacts.palette}|${this.props.clusterLevel ?? ''}|${highlightOrdinal}`
      );
    }
    timings.lutMs = performance.now() - lutStarted;

    // A refusal draws no marks but keeps the slab: the held bands are still the answer to the last
    // view that succeeded, and discarding them would make recovery pay for a full rewrite. So a
    // refused view is blank — the map element paints the state over it — and never an empty corpus.
    if (r.status === 'refused' || !r.marks || r.marks.bands.length === 0 && r.marks.standIn.length === 0 && !r.marks.count.exact) {
      if (!r.marks) slab.clear();
      this.props.onDrawn?.(0, 0);
      // Every sublayer, empty: the programs link now, during the wait for the first response,
      // and the first paint with marks pays no shader compile (see `warmMarksLayers`).
      return [...this.outlineLayers(r, timings), this.washLayer(null, 0), ...this.warmMarksLayers(), ...this.labelLayers(r, timings), ...this.selectionLayers()];
    }

    // The slab's colour attribute holds the **column** colouring. Colouring by cluster is the
    // texture and a uniform: the attribute keeps the last column encoding untouched, so the switch
    // back is free and the switch there rewrites nothing per point.
    const colourBy = r.legend?.colourBy ?? null;
    const clusterLayer = clusterLayerOf(colourBy);
    let column = heldColumnEncoding.get(slab) ?? {encoding: {kind: 'uniform'} as Encoding, colourBy: null};
    if (!clusterLayer) {
      column = {encoding: encodingOf(r.meta, r.legend), colourBy};
      heldColumnEncoding.set(slab, column);
    }
    const encoding = column.encoding;
    const encodingKey = encodingSignature(encoding);
    // The ordinals every mark carries are the first layer on's, whichever colouring is chosen, so
    // choosing cluster colour later is the uniform flip and not a rewrite of the attribute.
    const membershipLayer = clusterLayer ?? r.artifacts?.layers[0] ?? '';
    const useLut = clusterLayer !== null && lut.gpu !== null;
    const slabStarted = performance.now();
    slab.sync(r.marks.bands, r.depth, encoding, column.colourBy, membershipLayer);
    timings.slabMs = performance.now() - slabStarted;

    if (!checkedMarks.has(r.marks)) {
      checkedMarks.add(r.marks);
      // Every exact band the frame draws must have reached the slab: a band written outside its
      // slot, or a slot gone stale under a partition change, would thin the picture silently.
      for (const band of r.marks.bands) {
        if (!slab.holds(band)) {
          throw new Error(`TesseraLayer: exact band ${band.prefix} at depth ${band.depth} is drawn but has no slab slot.`);
        }
      }
    }

    if (this.props.wash) {
      const washStarted = performance.now();
      layers.push(this.washLayer(r.tiles, r.depth));
      timings.washMs = performance.now() - washStarted;
    }

    // The marks' size and alpha follow the resident count and the zoom (`markStyle`): small and
    // translucent at a million so density reads through them, larger and more solid as the count
    // falls. A style change is two uniforms, never a pass over the points.
    const standIn = this.standInBuffers(r.marks, column.colourBy, membershipLayer);
    const style = markStyle(slab.drawn + standIn.count, this.context.viewport?.zoom ?? 0, this.props.radius ?? null);
    const opacity = deckOpacity(style.alpha);
    timings.markRadius = style.radius;
    timings.markAlpha = style.alpha;
    timings.markCount = slab.drawn + standIn.count;

    // Layers toggle `visible`; they are never omitted — deck destroys an absent layer and re-uploads
    // everything it held when it returns. One layer per retained slab partition, addressed by slot,
    // so a depth flip is a swap and flipping back uploads nothing.
    const partitions = slab.layers();
    // The slab's own warm layer only: the stand-in layer is added below whatever the partitions
    // hold, and pushing the warm one here too gave deck two layers under `marks-standin`, which it
    // warned about and resolved by keeping one of them.
    if (partitions.length === 0) layers.push(...this.warmMarksLayers(false));
    for (const held of partitions) {
      layers.push(
        new MarksLayer(
          this.getSubLayerProps({id: `marks-p${held.slot}`}),
          {
            visible: held.active && held.draw.length > 0,
            data: {
              length: held.draw.length,
              attributes: held.draw.gpu
                ? gpuAttributes(held.draw.gpu)
                : {getPosition: binary(held.draw.positions, 2), getFillColor: binary(held.draw.colours, 4, true), getOrdinal: binary(held.draw.ordinals, 1)}
            },
            tesseraIds: held.draw.ids,
            useLut,
            lutTexture: lut.gpu,
            radiusUnits: 'pixels' as const,
            getRadius: style.radius,
            radiusMinPixels: 1,
            antialiasing: style.antialiasing,
            opacity,
            pickable: this.props.pickable && held.active,
            parameters: {depthCompare: 'always' as const}
          } as never
        )
      );
    }

    // The stand-ins: drawn at the same alpha as any other mark. What guards the reading is the
    // number channel — no count is shown against a non-exact tile — not the alpha channel.
    //
    // **They carry their ordinal and colour through the same texture as any other mark.** A
    // stand-in oversamples the ground it covers, but each mark in it is a real point of a real
    // band carrying the ordinal the response that served it named, so its colour is exact in
    // §5.10's sense. Drawing them neutral put a grey band over every tile the deeper cut had not
    // reached yet — up to 56% of the marks on screen mid-zoom on the 2.4M corpus — and that grey
    // is what read as colour reloading on a zoom in.
    const colours = this.standInColours(standIn, encoding, encodingKey);
    if (colours.length !== standIn.count * 4) {
      throw new Error(
        `colour buffer covers ${colours.length / 4} of ${standIn.count} stand-in marks. Colour is presentation and must never decide what is drawn.`
      );
    }
    layers.push(
      new MarksLayer(
        this.getSubLayerProps({id: 'marks-standin'}),
        {
          visible: standIn.count > 0,
          data: {
            length: standIn.count,
            attributes: {
              getPosition: binary(standIn.positions, 2),
              getFillColor: binary(colours, 4, true),
              getOrdinal: binary(standIn.ordinals, 1)
            }
          },
          tesseraIds: standIn.ids,
          useLut,
          lutTexture: lut.gpu,
          radiusUnits: 'pixels' as const,
          getRadius: style.radius,
          radiusMinPixels: 1,
          antialiasing: style.antialiasing,
          opacity,
          pickable: this.props.pickable,
          parameters: {depthCompare: 'always' as const}
        } as never
      )
    );

    this.props.onDrawn?.(slab.drawn, standIn.count);
    // The outlines go under everything: the marks show through the hovered one's faint fill.
    layers.unshift(...this.outlineLayers(r, timings));
    layers.push(...this.labelLayers(r, timings), ...this.selectionLayers());
    return layers;
  }

  /**
   * The mark layers with nothing in them — the first partition's slot and the stand-ins — so
   * that their programs are linked at the first paint of the session rather than at the first
   * paint with marks. luma links synchronously when a pipeline is made (its shader-layout
   * introspection forces the link to complete), and every program of this composite cost
   * 130–190 ms on the main thread at the moment the first million marks arrived — the largest
   * single block in that paint. An empty layer draws nothing and uploads nothing; a layer that
   * later fills keeps its id, so deck updates it rather than making it again.
   */
  private warmMarksLayers(standIn = true): Layer[] {
    const layers: Layer[] = [
      new MarksLayer(
        this.getSubLayerProps({id: 'marks-p0'}),
        {
          visible: false,
          data: {length: 0, attributes: {getPosition: binary(EMPTY_F32, 2), getFillColor: binary(EMPTY_U8, 4, true), getOrdinal: binary(EMPTY_F32, 1)}},
          tesseraIds: EMPTY_IDS,
          useLut: false,
          lutTexture: null,
          radiusUnits: 'pixels' as const,
          getRadius: this.props.radius ?? 1.6,
          pickable: false,
          parameters: {depthCompare: 'always' as const}
        } as never
      )
    ];
    if (standIn) {
      layers.push(
        new MarksLayer(
          this.getSubLayerProps({id: 'marks-standin'}),
          {
            visible: false,
            data: {length: 0, attributes: {getPosition: binary(EMPTY_F32, 2), getFillColor: binary(EMPTY_U8, 4, true), getOrdinal: binary(EMPTY_F32, 1)}},
            tesseraIds: EMPTY_IDS,
            useLut: false,
            lutTexture: null,
            radiusUnits: 'pixels' as const,
            getRadius: this.props.radius ?? 1.6,
            pickable: false,
            parameters: {depthCompare: 'always' as const}
          } as never
        )
      );
    }
    return layers;
  }

  private standInBuffers(marks: MarksProjection, colourBy: string | null, layer: string): StandInBuffers {
    const key = `${colourBy ?? ''}|${layer}`;
    const held = heldStandIn.get(marks.standIn);
    if (held && held.key === key) return held.buffers;
    const buffers = materialiseStandIn(marks.standIn, colourBy ? [colourBy] : [], layer);
    heldStandIn.set(marks.standIn, {key, buffers});
    return buffers;
  }

  private standInColours(standIn: StandInBuffers, encoding: Encoding, key: string): Uint8Array {
    const held = heldStandInColours.get(standIn);
    if (held && held.key === key) return held.colours;
    const colours = buildColourAttribute(standIn.count, standIn.scalars, encoding);
    heldStandInColours.set(standIn, {key, colours});
    return colours;
  }

  /**
   * The density wash, rebuilt once per `tiles` object — and **off the paint path**. Binning and
   * filtering the wash over a full viewport's tiles (65,536 at depth 8) is 60–70 ms, and it sat
   * inside the first paint with marks. Now a new `tiles` object schedules the build on a
   * macrotask, the paint draws the previous wash (or none) meanwhile, and the layer asks for an
   * update when the image is ready — one frame later, never inside the frame that draws the
   * marks. The image still comes from the exact tiles' counts and nothing else (§5.10).
   *
   * `tiles` null draws the empty layer, so the bitmap program links with the rest at the first
   * paint of the session.
   */
  private washLayer(tiles: TilesProjection | null, depth: number): Layer {
    let image: ImageData | null = null;
    let bounds: [number, number, number, number] = [0, 0, 1, 1];
    if (tiles) {
      let held = heldWash.get(tiles);
      if (!held || held.depth !== depth) {
        if (!held || !held.pending) {
          // Keep the last wash drawn while this one is built: no flash to nothing at a settle.
          held = {depth, image: lastWash?.image ?? null, bounds: lastWash?.bounds ?? [0, 0, 1, 1], pending: true};
          heldWash.set(tiles, held);
          const build = () => {
            washTimer = null;
            const binned = binDensity(tiles.tiles, depth);
            const built = binned && binned.filled > 0 ? filterDensity(binned, depth) : null;
            const entry = {
              depth,
              image: built && typeof ImageData !== 'undefined' ? new ImageData(built.data, built.width, built.height) : null,
              bounds: built ? built.bounds : ([0, 0, 1, 1] as [number, number, number, number]),
              pending: false
            };
            heldWash.set(tiles, entry);
            lastWash = entry;
            // The layer that is current for this id, which may no longer be this instance.
            const current = (this.getCurrentLayer?.() as TesseraLayer | null) ?? this;
            // A discarded instance has no manager to ask; the next paint reads the memo anyway.
            if (!current.lifecycle || /Discarded|Finalized/.test(String(current.lifecycle))) return;
            current.setNeedsUpdate();
            current.setNeedsRedraw();
          };
          // Debounced to the settle: while a response streams in, every fold hands the layer a
          // new `tiles` object a frame apart, and a wash per frame would cost more than the
          // marks it sits under. The last one asked for is the one built.
          if (typeof setTimeout !== 'undefined') {
            if (washTimer !== null) clearTimeout(washTimer);
            washTimer = setTimeout(build, WASH_SETTLE_MS);
          } else build();
        }
      }
      image = held.image;
      bounds = held.bounds;
    }
    const [x0, y0, x1, y1] = bounds;
    return new BitmapLayer(
      this.getSubLayerProps({id: 'wash'}),
      {
        visible: image !== null,
        image: image ?? EMPTY_IMAGE,
        // `[left, bottom, right, top]`: row 0 of the image is the lowest tile row, and under the
        // y-down orthographic view that is the smaller world y — so `top` is `y0`.
        bounds: [x0, y1, x1, y0],
        pickable: false,
        // Linear, over the filtered field: the tile grid is never drawn (decision 0097).
        textureParameters: {minFilter: 'linear', magFilter: 'linear'},
        parameters: {depthCompare: 'always' as const}
      } as never
    );
  }

  /**
   * The served shape's parts — or the `box`, where the layer draws no shape — for the hovered and
   * the opened artifact, in its own colour, the opened one strong with a faint fill
   * ({@link focusOutlines}), every kind through this one path. A derived shape is per principal
   * (contracts §3.2), so it is exact for this viewer; nothing is contoured from held marks
   * (decision 0099).
   *
   * **Nothing else is in this layer.** The rest of the frontier used to sit here at zero alpha to
   * answer deck's pick, which put every served ring through the tessellator on every hover change;
   * a pick over a contour is resolved in JS instead, against {@link contourShapes}, and a click
   * takes the same route as the hover (`map.ts`'s `onClick`). So this layer is not pickable and
   * carries no `artifactIds`: a label still answers deck's pick and names its artifact, and that is
   * now the only route by which `resolvePick` reports one.
   *
   * The shapes do not depend on the zoom, so the memo survives a zoom that re-places the labels.
   * It is keyed on the fetched shapes as well as the served set, because a shape arriving by
   * identifier changes neither the served array nor the projection's version.
   */
  private outlineLayers(r: Resolved, timings: {outlinesMs: number; outlines: number; outlinesDrawn: number}): Layer[] {
    const a = r.artifacts;
    const started = performance.now();
    const opened = this.props.openedArtifact ?? null;
    const hovered = this.props.hoveredArtifact ?? null;
    const scheme = this.props.scheme ?? 'dark';
    const key = a ? `${a.version}|${a.palette}|${opened ?? ''}|${hovered ?? ''}|${scheme}|${this.props.clusterLevel ?? ''}` : '';
    let held = a ? heldOutlines.get(a.served) : undefined;
    if (a && (!held || held.key !== key || held.shapes !== a.shapes)) {
      held = {key, shapes: a.shapes, data: focusOutlines(a, {opened, hovered, level: this.props.clusterLevel, scheme, meta: r.meta})};
      heldOutlines.set(a.served, held);
    }
    const data = held?.data ?? NO_OUTLINES;
    timings.outlinesMs = performance.now() - started;
    timings.outlines = data.length;
    timings.outlinesDrawn = new Set(data.map((d) => d.id)).size;
    // The layer exists from the first paint, empty, so its program is linked before it is needed.
    return [
      new PolygonLayer(
        this.getSubLayerProps({id: 'outlines'}),
        {
          visible: data.length > 0,
          data,
          getPolygon: (d: OutlineDatum) => d.polygon,
          // A box draws unfilled, which is the whole of what `fill` carries here — every row in
          // this layer is a shape that draws.
          filled: true,
          getFillColor: (d: OutlineDatum) => [d.colour[0], d.colour[1], d.colour[2], d.fill],
          stroked: true,
          getLineColor: (d: OutlineDatum) => [d.colour[0], d.colour[1], d.colour[2], d.line],
          lineWidthUnits: 'pixels' as const,
          getLineWidth: (d: OutlineDatum) => d.width,
          lineWidthMinPixels: 0.8,
          pickable: false,
          parameters: {depthCompare: 'always' as const},
          updateTriggers: {getFillColor: key, getLineColor: key, getLineWidth: key}
        } as never
      )
    ];
  }

  /**
   * Names and counts at each artifact's `centroid`, sized by masked count on a logarithmic band,
   * placed by priority into a spatial hash — a few hundred fit a viewport and the rest wait for
   * a zoom — with a leader line where a label moved. A dependent artifact draws at its own
   * declared centroid with the count the wire carries for it (its target's, D13). Free text is
   * deck's `TextLayer` with `characterSet: 'auto'` and an SDF halo.
   */
  private labelLayers(r: Resolved, timings: {labelsMs: number; labels: number}): Layer[] {
    const a = this.props.labels ? r.artifacts : null;
    const viewport = this.context.viewport;
    const started = performance.now();
    const zoom = viewport?.zoom ?? 0;
    const bucket = Math.round(zoom * LABEL_ZOOM_STEP);
    const budget = viewport ? labelBudget(viewport.width, viewport.height) : 0;
    const key = a ? `${a.version}|${a.palette}|${bucket}|${budget}|${this.props.clusterLevel ?? ''}` : '';
    let held = a ? heldLabels.get(a.served) : undefined;
    if (a && viewport && (!held || held.key !== key)) {
      const scale = 2 ** zoom; // pixels per world unit
      const {candidates, byId} = labelCandidates(a, r.meta, this.props.clusterLevel, zoom, budget);
      const data: LabelDatum[] = [];
      const leaders: LeaderDatum[] = [];
      let placed = 0;
      for (const p of placeLabels(candidates) as PlacedLabel[]) {
        placed += 1;
        const {artifact, lines, countText, size, topic} = byId.get(p.id)!;
        const position = gridToWorldXY(artifact.centroid!);
        const ordinal = a.table.ordinalOf(artifact.layer, artifact.tesseraId);
        const colour = a.colours.get(ordinal) ?? NEUTRAL;
        // The name over up to three lines, centred on the anchor; the count beside the **last**
        // line — smaller and lighter — then the topic beneath the block. The last line ends and
        // the count starts at one seam, so the two width estimates cannot overlap.
        const step = size * LABEL_LINE_HEIGHT;
        const top = p.dy - ((lines.length - 1) * step) / 2;
        const last = lines[lines.length - 1]!;
        const lastWidth = last.length * NAME_EM * size;
        const countWidth = countText.length * COUNT_EM * size * COUNT_SCALE;
        const seam = p.dx - (lastWidth + countWidth + size * COUNT_GAP_EM) / 2 + lastWidth;
        lines.forEach((line, i) => {
          const isLast = i === lines.length - 1;
          data.push({
            id: artifact.tesseraId,
            position,
            text: line,
            size,
            offset: [isLast ? seam : p.dx, top + i * step],
            colour,
            kind: 'name',
            anchor: isLast ? 'end' : 'middle'
          });
        });
        const baseline = top + (lines.length - 1) * step;
        data.push({id: artifact.tesseraId, position, text: countText, size: size * COUNT_SCALE, offset: [seam + size * COUNT_GAP_EM, baseline + size * 0.08], colour, kind: 'count', anchor: 'start'});
        if (topic) data.push({id: artifact.tesseraId, position, text: topic, size: TOPIC_SIZE, offset: [p.dx, baseline + size * 0.78 + 3], colour, kind: 'topic', anchor: 'middle'});
        // A leader wherever the label moved: the placement bounds the move (`MAX_DISPLACEMENT`),
        // so a leader is a short tie to the centroid and never a line across the map.
        if (p.leader) leaders.push({from: position, to: [position[0] + p.dx / scale, position[1] + p.dy / scale]});
      }
      held = {key, data, leaders, placed};
      heldLabels.set(a.served, held);
    }
    const data = held?.data ?? NO_LABELS;
    const leaders = held?.leaders ?? NO_LEADERS;
    timings.labelsMs = performance.now() - started;
    // Labels placed, not text rows: a wrapped name is several rows of one label.
    timings.labels = held?.placed ?? 0;
    // Both layers exist from the first paint, empty, so their programs are linked before needed.
    const layers: Layer[] = [];
    {
      layers.push(
        new LineLayer(
          this.getSubLayerProps({id: 'label-leaders'}),
          {
            visible: leaders.length > 0,
            data: leaders,
            getSourcePosition: (d: LeaderDatum) => d.from,
            getTargetPosition: (d: LeaderDatum) => d.to,
            getColor: [...INK[this.props.scheme ?? 'dark'], 100] as [number, number, number, number],
            updateTriggers: {getColor: this.props.scheme},
            widthUnits: 'pixels' as const,
            getWidth: 1,
            pickable: false,
            parameters: {depthCompare: 'always' as const}
          } as never
        )
      );
    }
    const scheme = this.props.scheme ?? 'dark';
    const ink = [...INK[scheme], 255] as [number, number, number, number];
    const text = (id: string, kind: LabelDatum['kind'], rows: LabelDatum[], extra: Record<string, unknown>) =>
      new TextLayer(
        this.getSubLayerProps({id}),
        {
          visible: rows.length > 0,
          data: rows,
          getPosition: (d: LabelDatum) => d.position,
          getText: (d: LabelDatum) => d.text,
          getSize: (d: LabelDatum) => d.size,
          sizeUnits: 'pixels' as const,
          getColor: kind === 'name' ? ink : ([ink[0], ink[1], ink[2], kind === 'count' ? 184 : 178] as [number, number, number, number]),
          getPixelOffset: (d: LabelDatum) => d.offset,
          getTextAnchor: (d: LabelDatum) => d.anchor,
          getAlignmentBaseline: 'center' as const,
          fontFamily: 'IBM Plex Sans, system-ui, -apple-system, Segoe UI, Roboto, sans-serif',
          // **The halo is the distance field's, so it is bounded by the atlas's padding.** deck
          // scales `outlineWidth` by `fontSettings.radius` and clips the field at `buffer` glyph
          // pixels; with its defaults (buffer 4 at a 64 px atlas) the widest outline a 12 px name
          // could draw was about a third of a pixel, whatever `outlineWidth` said — which is why
          // the names read as unhaloed over the marks. A padded atlas buys the boards' outline
          // (`gen.py` paints its labels with a stroke of 0.32 em under the fill).
          fontSettings: {sdf: true, buffer: 12, radius: 12, cutoff: 0.25},
          outlineWidth: HALO_EM * 64,
          outlineColor: HALO[scheme],
          characterSet: 'auto',
          pickable: this.props.pickable && kind === 'name',
          artifactIds: rows.map((d) => d.id),
          parameters: {depthCompare: 'always' as const},
          updateTriggers: {getPixelOffset: key, getSize: key, getColor: scheme},
          ...extra
        } as never
      );
    layers.push(
      text('labels', 'name', data.filter((d) => d.kind === 'name'), {fontWeight: 600}),
      text('label-counts', 'count', data.filter((d) => d.kind === 'count'), {fontWeight: 400}),
      text('label-topics', 'topic', data.filter((d) => d.kind === 'topic'), {fontWeight: 400, fontStyle: 'italic'})
    );
    return layers;
  }

  /** The picked mark's marker, and the selected region as the shape drawn — never its cells. */
  private selectionLayers(): Layer[] {
    const layers: Layer[] = [];
    const box = this.props.drag ?? this.props.region ?? null;
    const polygon = this.props.dragPolygon ?? this.props.regionPolygon ?? null;
    const shape: [number, number][] | null =
      polygon && polygon.length >= 2 ? polygon : box ? [[box[0], box[1]], [box[2], box[1]], [box[2], box[3]], [box[0], box[3]]] : null;
    const live = this.props.drag != null || this.props.dragPolygon != null;
    const scheme = this.props.scheme ?? 'dark';
    const accent = ACCENT[scheme];
    // Both layers exist from the first paint, empty, so their programs are linked before needed.
    {
      layers.push(
        new PolygonLayer(
          this.getSubLayerProps({id: 'region'}),
          {
            visible: shape !== null,
            data: shape ? [{polygon: shape}] : NO_SHAPES,
            getPolygon: (d: {polygon: number[][]}) => d.polygon,
            filled: (shape?.length ?? 0) >= 3,
            getFillColor: [...accent, live ? 20 : 30] as [number, number, number, number],
            stroked: true,
            getLineColor: [...accent, live ? 255 : 230] as [number, number, number, number],
            lineWidthUnits: 'pixels' as const,
            getLineWidth: 1.5,
            pickable: false,
            parameters: {depthCompare: 'always' as const},
            updateTriggers: {getFillColor: [live, scheme], getLineColor: [live, scheme]}
          } as never
        )
      );
    }
    const at = this.props.selectedWorldXY;
    {
      layers.push(
        new ScatterplotLayer(
          this.getSubLayerProps({id: 'picked'}),
          {
            visible: at !== null && at !== undefined,
            data: at ? [at] : NO_POINTS,
            getPosition: (d: [number, number]) => d,
            getFillColor: [0, 0, 0, 0],
            radiusUnits: 'pixels' as const,
            getRadius: 6,
            stroked: true,
            getLineColor: [...INK[scheme], 255] as [number, number, number, number],
            lineWidthUnits: 'pixels' as const,
            getLineWidth: 2,
            updateTriggers: {getLineColor: scheme},
            pickable: false,
            parameters: {depthCompare: 'always' as const}
          } as never
        )
      );
    }
    return layers;
  }
}
