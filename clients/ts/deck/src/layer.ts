import {CompositeLayer, type BinaryAttribute as DeckBinaryAttribute, type CompositeLayerProps, type Layer, type LayersList, type UpdateParameters} from '@deck.gl/core';
import {BitmapLayer, LineLayer, PolygonLayer, ScatterplotLayer, TextLayer} from '@deck.gl/layers';
import {
  CLUSTER_PREFIX,
  NO_ORDINAL,
  artifactName,
  WORLD_SIZE,
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
  type Store
} from '@tesseradb/client';
import {NEUTRAL} from '@tesseradb/client/internal';
import {materialiseStandIn, type StandInBuffers} from './assemble.js';
import {DEFAULT_COLOURING, buildColourAttribute, encodingSignature, rampAt as rampAtStops, rgbOfHex, type Colouring, type Encoding, type Rgb} from './colour.js';
import {shapeBbox, smoothRing, type ContourShape, type Part} from './contours.js';
import {DEFAULT_DENSITY_SCALE, binDensity, contourThresholds, densityPaint, densityPosition, densityStops, drawnCells, filterDensity, gridImage, maxCount, type DensityCell, type DensityColours, type DensityCounts, type DensityMode, type DensityScale} from './density.js';
import {DEFAULT_DENSITY_CELL_PX, DensityCounter} from './density-counter.js';
import {LABEL_LINE_HEIGHT, labelLine, labelSize, placeLabels, type LabelCandidate, type PlacedLabel} from './labels.js';
import {importAggregation} from './aggregation-loader.js';
import {LookupTexture} from './lut.js';
import {DULL_COLOUR, MarksLayer, type HighlightPass} from './marks-layer.js';
import {deckOpacity, markStyle} from './marks-style.js';
import {DEFAULT_SIZING, buildSizeAttribute, drawnSizing, sizeEncodingOf, sizeSignature, sizingRadius, type SizeEncoding, type Sizing} from './size.js';
import {MarkSlab, type GpuSlab} from './slab.js';

/**
 * The props of a {@link TesseraLayer}: deck.gl's `CompositeLayerProps` and the props below, all
 * optional. `pickable` defaults to true. Set `store`, or set the projection props (`marks`,
 * `depth`, `artifacts`, `meta`, `legend`, `status`) yourself. A projection prop set beside
 * `store` is drawn in place of the store's.
 */
export type TesseraLayerProps = CompositeLayerProps & {
  /** The store to read projections from. The layer subscribes to it and redraws on each change. Defaults to `null`. */
  store?: Store | null;
  /** The marks to draw, in place of the store's `marks`. Defaults to `null`. */
  marks?: MarksProjection | null;
  /**
   * The depth of the frame's bands, which chooses the mark buffers drawn. Defaults to 0, which with
   * `store` reads the store's `view.depth`.
   */
  depth?: number;
  /** The served artifacts to name, outline and colour by, in place of the store's `artifacts`. Defaults to `null`. */
  artifacts?: ArtifactsProjection | null;
  /**
   * The `/v1/meta` answer, in place of the store's `meta`. Column colour reads its columns and the
   * labels and outlines read its layers; without it the marks draw in one colour. Defaults to
   * `null`.
   */
  meta?: Meta | null;
  /**
   * The colouring and sizing, in place of the store's `legend`. Its `colourBy` names a column, or
   * `cluster:<layer>` to colour each mark by the artifact it belongs to, and its `sizeBy` a number
   * column to size each mark by. Defaults to `null`, which draws every mark in one colour and one
   * size.
   */
  legend?: LegendProjection | null;
  /**
   * The display status, in place of the store's `status`. Under `refused` no marks are drawn.
   * Defaults to `idle`, which with `store` reads the store's.
   */
  status?: PresentedStatus;
  /**
   * The level of a nested layer to colour, name and outline artifacts at. Defaults to `undefined`,
   * the deepest level served.
   */
  clusterLevel?: number;
  /** Whether the artifacts' names are drawn at their centroids, with the hovered one's count. Defaults to true. */
  labels?: boolean;
  /**
   * The ground under the map, `light` or `dark`. The ink and halo of names, the hovered outline's
   * fill and the colours of the picked mark's ring and the selected region follow it. Defaults to
   * `dark`.
   */
  scheme?: 'light' | 'dark';
  /**
   * The picked mark's position in world units, where a ring is drawn. {@link resolvePick} gives it
   * as `worldXY`. Defaults to `null`, which draws no ring.
   */
  selectedWorldXY?: [number, number] | null;
  /**
   * The `tesseraId` of the opened artifact, whose outline is drawn. Under cluster colour, the marks
   * coloured by any other artifact, or by none, draw dimmed. Defaults to `null`.
   */
  openedArtifact?: bigint | null;
  /** The `tesseraId` of the artifact under the pointer, whose outline is drawn. Defaults to `null`. */
  hoveredArtifact?: bigint | null;
  /** The selected region as a box `[x0, y0, x1, y1]` in world units. Defaults to `null`. */
  region?: [number, number, number, number] | null;
  /**
   * The selected region as a polygon of world points. With two or more points it is drawn in place
   * of `region` and of `drag`. Defaults to `null`.
   */
  regionPolygon?: [number, number][] | null;
  /**
   * The box being dragged, `[x0, y0, x1, y1]` in world units, drawn in place of `region` with a
   * fainter fill and an opaque line. Defaults to `null`.
   */
  drag?: [number, number, number, number] | null;
  /**
   * The lasso being drawn, as world points, drawn in place of `regionPolygon` and `drag` with a
   * fainter fill and an opaque line. Defaults to `null`.
   */
  dragPolygon?: [number, number][] | null;
  /**
   * Whether the request carried a highlight. When true, the marks that satisfy it keep their
   * colour, draw larger and carry a soft glow of their own colour, and the rest draw light grey and
   * faint. The layer does not read this from `store`: pass the store's `view.highlighting`.
   * Defaults to false.
   */
  highlighting?: boolean;
  /** Whether the marks are drawn. Off, only density, outlines, labels and the selection draw. Defaults to true. */
  points?: boolean;
  /**
   * A fixed mark radius in pixels. Defaults to `null`, which sizes marks by how many are drawn and
   * by the zoom: 1.7 px at a hundred marks or fewer down to 1.1 px at a million, plus 0.05 px per
   * zoom level up to zoom 10. Under the legend's `sizeBy`, `sizing` sizes the marks in its place.
   */
  radius?: number | null;
  /**
   * The smallest and largest radius and the scale a number column sizes marks on, under the
   * legend's `sizeBy`. A mark with no value, NaN or an infinity draws as a ring, at the smallest
   * radius or 3 px, whichever is larger. A radius that is not a finite number above zero is the
   * default's. Under `rank` the store keeps the sample ranked against only where it was asked to,
   * with `setSizeBy(column, {rank: true})`. Defaults to {@link DEFAULT_SIZING}.
   */
  sizing?: Sizing;
  /**
   * A fixed mark alpha from 0 to 1, as a fraction of the colour's own alpha. Defaults to `null`,
   * which sets it by how many marks are drawn: 0.78 at a hundred or fewer down to 0.34 at a
   * million, plus a little per zoom level.
   */
  pointOpacity?: number | null;
  /**
   * How density is drawn under the marks, from `densityCounts`: `none`, `smooth` (a soft wash),
   * `hex` (hexagons), `grid` (square cells one cell wide) or `contours` (lines of equal density).
   * Defaults to `smooth`.
   */
  density?: DensityMode;
  /**
   * The counts by cell density is drawn from, in place of `densityCounter`'s. A new object rebuilds
   * the smooth wash or the grid outside the render, and the previous one is drawn until then.
   * Defaults to `null`.
   */
  densityCounts?: DensityCounts | null;
  /**
   * The {@link DensityCounter} whose counts density is drawn from, where the host keeps one, as
   * `<tessera-map>` does. Defaults to `null`: with `store` set and no `densityCounts`, the layer
   * keeps a counter of its own over `store`, told of the layer's viewport and of `density` and
   * `densityResolution`.
   */
  densityCounter?: DensityCounter | null;
  /**
   * The cell size on screen, in CSS pixels, the layer's own counter asks for; see
   * {@link DensityCounter}. Defaults to 12.
   */
  densityResolution?: number;
  /**
   * The colours density is drawn in. Defaults to `null`, which draws the smooth wash in warm grey
   * while the marks are drawn, and every other case in Viridis.
   */
  densityColours?: DensityColours | null;
  /** How strongly density is drawn, from 0 (not at all) to 1 (fully). Defaults to 1. */
  densityStrength?: number;
  /**
   * How a cell's count is placed between no items and the largest count drawn, which picks its
   * colour, its alpha in the wash and the contour levels: `linear` or `log`. Defaults to `log`.
   */
  densityScale?: DensityScale;
  /**
   * The palette, ramp and chosen value colours column colour uses. Defaults to
   * {@link DEFAULT_COLOURING}.
   */
  colouring?: Colouring;
  /**
   * Called each time the layer renders, with the marks it drew: `drawn` exact marks held for the
   * frame, and `provisional` stand-ins drawn until the exact bands arrive. Called with `(0, 0)`
   * when no marks are drawn. Defaults to `null`.
   */
  onDrawn?: ((drawn: number, provisional: number) => void) | null;
  /** Called each time the layer renders, with what the render cost and drew. Defaults to `null`. */
  onTimings?: ((t: LayerTimings) => void) | null;
};

/**
 * What one render of a {@link TesseraLayer} cost and drew, passed to `onTimings`. Times are in
 * milliseconds. On a render that draws no marks, `slabMs`, `densityMs` and the mark figures are 0.
 */
export type LayerTimings = {
  /** Time spent writing new bands into the mark buffers. */
  slabMs: number;
  /**
   * Time spent on the density layers. The smooth wash's image is built later, outside the render,
   * and the other modes aggregate inside deck.gl's own update; neither is counted.
   */
  densityMs: number;
  /** Time spent updating the cluster colour lookup texture. */
  lutMs: number;
  /** Time spent building the outlines. */
  outlinesMs: number;
  /** Time spent placing labels. */
  labelsMs: number;
  /** Time for the whole render, every stage included. */
  layersMs: number;
  /** Writes to the cluster colour lookup texture since the layer made it. */
  lutWrites: number;
  /** Outline parts drawn. An artifact in several pieces has several parts. */
  outlines: number;
  /** Artifacts outlined: 0, 1 or 2 (the hovered and the opened). */
  outlinesDrawn: number;
  /** Labels placed. */
  labels: number;
  /** The mark radius drawn, in pixels; under sizing by a column, the largest. */
  markRadius: number;
  /** The alpha the marks are composited at, from 0 to 1, as a fraction of their colour's own alpha. */
  markAlpha: number;
  /** The number of marks the radius and alpha were chosen for: the exact marks held and the stand-ins. */
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
  depth: number;
  artifacts: ArtifactsProjection | null;
  meta: Meta | null;
  legend: LegendProjection | null;
  status: PresentedStatus;
};

/** The selected region's colour per ground: the interface's ink, since colour on the map is data's. */
const ACCENT: Record<'light' | 'dark', [number, number, number]> = {light: [27, 29, 33], dark: [236, 238, 241]};
/** Label ink per ground. */
const INK: Record<'light' | 'dark', [number, number, number]> = {light: [27, 29, 33], dark: [242, 243, 245]};
/** The label halo per ground: the map's default background, nearly opaque. */
const HALO: Record<'light' | 'dark', [number, number, number, number]> = {light: [246, 246, 244, 235], dark: [17, 19, 23, 235]};
/**
 * The halo's width as a fraction of the em: 2 px on a 14 px name, the middle of the size band. The
 * distance field's reach ({@link HALO_RADIUS}) bounds the width it can draw; asking for more fills
 * each glyph's cell as a rectangle. 0.14 em is half the reach.
 */
const HALO_EM = 0.14;
/** The em the SDF atlas is baked at: deck's `fontSettings.fontSize` default. */
const ATLAS_PX = 64;
/**
 * The distance field's reach in atlas pixels, and the padding around each glyph. deck puts the
 * outline threshold at `0.75 × (1 − outlineWidth / radius)`, so `outlineWidth / radius` must stay
 * well below 1. The padding need only cover the halo (9 px), and each pixel of it costs atlas
 * area per glyph.
 */
const HALO_RADIUS = 24;
const HALO_BUFFER = 14;
/** deck's `outlineWidth` for a halo of {@link HALO_EM}: deck draws `0.75 ×` this in atlas pixels. */
const HALO_OUTLINE_WIDTH = (HALO_EM * ATLAS_PX) / 0.75;
/**
 * The glyphs an empty text layer is given in place of `'auto'`, which over no rows builds a font
 * atlas 0 px high that WebGL refuses to upload.
 */
const WARM_GLYPHS = '0123456789';
const CHROME: [number, number, number, number] = [234, 238, 243, 240];
const PLATE: [number, number, number, number] = [13, 15, 18, 235];

/**
 * The current column colour encoding, from the store's legend, the schema and the colour choices.
 * Uniform where nothing is ready to colour by yet, such as a column chosen before any of its codes
 * were counted. Unmapped where `/v1/categories` refused the column, so the legend can say the
 * values could not be named. A chosen value colour applies once the legend has named its key.
 */
export function encodingOf(meta: Meta | null, legend: LegendProjection | null, colouring: Colouring = DEFAULT_COLOURING): Encoding {
  const colourBy = legend?.colourBy ?? null;
  if (!colourBy || !meta || !legend || colourBy.startsWith(CLUSTER_PREFIX)) return {kind: 'uniform'};
  const column = meta.declaredScalars.find((c) => c.name === colourBy);
  if (!column) return {kind: 'uniform'};
  if (legend.categoryErrors[colourBy]) return {kind: 'unmapped'};
  if (column.category) {
    // The ranks come from codes counted in held bands, so the map is coloured without waiting
    // for `/v1/categories`, which only names the legend's entries.
    const rankOfCode = legend.ranks[colourBy];
    if (!rankOfCode || Object.keys(rankOfCode).length === 0) return {kind: 'uniform'};
    const chosen = new Map<number, Rgb>();
    const picked = colouring.values[colourBy];
    if (picked) {
      for (const value of legend.categories[colourBy] ?? []) {
        const colour = picked[value.key] === undefined ? null : rgbOfHex(picked[value.key]!);
        if (colour) chosen.set(value.code, colour);
      }
    }
    return {kind: 'category', column: colourBy, rankOfCode, palette: colouring.palette, chosen};
  }
  const domain = legend.domains[colourBy];
  if (!domain) return {kind: 'uniform'};
  return {kind: 'numeric', column: colourBy, domain, ramp: colouring.ramp, scale: colouring.scale, reverse: colouring.reverse};
}

/**
 * A binary attribute object, reused while its array is the same object. deck.gl skips an upload
 * when the attribute object is the same reference, so a new literal each paint would re-upload
 * unchanged bytes.
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
 * Attribute objects around a partition's GPU buffers, keyed by attribute name so deck binds
 * each buffer without copying. Memoised on the `GpuSlab`, which is stable across appends.
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
      instanceOrdinals: {buffer: gpu.ordinals, size: 1, type: 'float32', stride: 4, offset: 0},
      instanceHighlights: {buffer: gpu.highlights, size: 1, type: 'float32', stride: 4, offset: 0},
      instanceSizes: {buffer: gpu.sizes, size: 1, type: 'float32', stride: 4, offset: 0}
    };
    gpuDescriptors.set(gpu, held);
  }
  return held;
}

/** The stand-in buffers, once per piece list. */
const heldStandIn = new WeakMap<object, {key: string; buffers: StandInBuffers}>();
/** The stand-in colours, once per (buffers, encoding). */
const heldStandInColours = new WeakMap<object, {key: string; colours: Uint8Array}>();
/** The stand-in size fractions, once per (buffers, size encoding). */
const heldStandInSizes = new WeakMap<object, {key: string; sizes: Float32Array}>();
/** `marks` objects whose slab-residency check has run. */
const checkedMarks = new WeakSet<object>();
/** What a density image is built for: one set of counts, drawn as a wash or a grid on one scale, in one set of colours on one ground. */
type WashKey = {counts: DensityCounts; kind: 'smooth' | 'grid'; scheme: 'light' | 'dark'; colours: DensityColours; scale: DensityScale};
/**
 * One layer's wash: the last image built, drawn until the next is ready, and the build waiting to
 * run. Held in the layer's state, so two maps on one page keep separate washes.
 */
type WashState = {
  built: (WashKey & {image: ImageData | null; bounds: [number, number, number, number]}) | null;
  pending: WashKey | null;
  timer: ReturnType<typeof setTimeout> | null;
};
const sameWash = (a: WashKey | null, b: WashKey) => a !== null && a.counts === b.counts && a.kind === b.kind && a.scheme === b.scheme && a.colours === b.colours && a.scale === b.scale;
/** The colours a hexagon or grid cell takes, sparse to dense, in equal steps of the scale. */
const DENSITY_STEPS = 8;
/** The share of a hexagon drawn, so neighbouring hexagons show a hairline gap. */
const DENSITY_COVERAGE = 0.94;

/** The aggregation layers the hexagons and contours need, once they have loaded. */
type Aggregation = Awaited<ReturnType<typeof importAggregation>>;
let aggregation: Aggregation | null = null;
let aggregationLoading: Promise<Aggregation> | null = null;

/**
 * Load `@deck.gl/aggregation-layers`, once. The hexagons and contours are drawn from it, and it is
 * loaded the first time either is asked for, so a host that never draws them does not load it.
 *
 * @internal
 */
export function loadAggregationLayers(): Promise<Aggregation> {
  aggregationLoading ??= importAggregation().then((m) => (aggregation = m));
  return aggregationLoading;
}
/** Shared empty inputs, so the empty sublayers' attribute objects are stable across paints. */
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
/** The drawn outlines, once per served set, fetched shapes, opened artifact and hovered artifact. */
const heldOutlines = new WeakMap<object, {key: string; shapes: object; data: OutlineDatum[]}>();
/** The label placement, once per served set and zoom bucket. */
const heldLabels = new WeakMap<object, {key: string; data: LabelDatum[]; leaders: LeaderDatum[]; placed: number}>();
/** The label candidates, once per served set, with anchors in world units; see {@link labelCandidates}. */
const heldCandidates = new WeakMap<object, {key: string; candidates: LabelCandidate[]; byId: Map<bigint, LabelText>}>();

/**
 * One drawn part of an artifact's shape: its outer ring and its holes, as deck's `PolygonLayer`
 * takes them. An artifact in several pieces is several rows with the same `id`, drawn alike.
 */
export type OutlineDatum = {
  id: bigint;
  polygon: [number, number][][];
  colour: Rgba;
  opened: boolean;
  hovered: boolean;
  /** The served `rung`: the artifact's level, or its depth in a tree. */
  rung: number;
  /** Which shape the wire answered with, and so whether the ring was smoothed ({@link outlineOf}). */
  source: OutlineSource;
  /** The fill and line alphas (0 to 255) and the line width in pixels this outline draws with. */
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
  /** Where the offset sits on the run: a name and a topic centre, and a count starts where the name ends. */
  anchor: 'start' | 'middle' | 'end';
};
type LeaderDatum = {from: [number, number]; to: [number, number]};

/** The hovered outline: a faint fill and a firm line, less than the opened one's. */
const HOVER_FILL: Record<'light' | 'dark', number> = {light: 26, dark: 33};
const HOVER_LINE = 150;
/** The opened outline: a 0.16 alpha fill and a strong line. */
const OPENED_FILL = 41;
const OPENED_LINE = 200;
/**
 * A `box` draws as an unfilled hairline rectangle, hovered or opened. A box is the bounds of the
 * visible members, so filling it would tint ground they need not occupy.
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

/** The level cut and the layer roster, which decide what may be hovered. */
export type ContourOptions = {
  level: number | undefined;
  meta?: Meta | null;
};

/** Whether a served artifact is drawn at `level` and has no drawn child: {@link frontier}'s rule for one artifact. */
function onFrontier(a: ArtifactsProjection, artifact: Artifact, level: number | undefined): boolean {
  const drawn = (x: {rung: number}) => level === undefined || x.rung <= level;
  if (!drawn(artifact)) return false;
  return !(a.lineage.childrenOf.get(artifact.tesseraId) ?? []).some((c) => drawn(c));
}

/** The layers whose artifacts attach their text to another layer's, and draw no shape of their own. */
function dependentLayers(meta: Meta | null | undefined): Set<string> {
  return new Set(meta?.layers.filter((l) => l.depsOn.length > 0).map((l) => l.name) ?? []);
}

/** The kind of shape a layer declares in `/v1/meta`, or null with no roster. */
function shapeKindOf(meta: Meta | null | undefined, layer: string): ShapeKind | null {
  return meta?.layers.find((l) => l.name === layer)?.shape ?? null;
}

/**
 * The shapes a viewer may point at, for {@link hoverAt}: one per served artifact in `frontier`,
 * with the served vertices (not the smoothed curve). A response also carries their ancestors,
 * which are not drawn and so are left out; so are a dependent layer's artifacts, which have no
 * shape of their own.
 *
 * An artifact's entry is its `box` until its served shape arrives: the viewport asks for
 * centroids and boxes, and the store fetches the shape of what the pointer lands on by
 * identifier. A pointer between two parts, or in a hole, is outside the shape.
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
 * The outlines drawn: the hovered artifact and the opened one, and no others. At rest the map is
 * colour and names; outlines for every served artifact would stack a frontier's shapes on its
 * ancestors'.
 *
 * On a layer that declares a shape, a box is a placeholder for a shape on its way and is not
 * drawn, so the outline does not change under the pointer when the shape lands. Only a derived
 * shape (a hull through member positions) is smoothed. A predicate or authored shape is a drawn
 * boundary, and a curve through it would move the border. A box is never smoothed, since four
 * corners through a spline make an oval.
 *
 * The result has one row per part, all parts of an artifact drawn alike, ordered by `rung` so an
 * opened child sits over an opened parent.
 */
export function focusOutlines(a: ArtifactsProjection, o: OutlineOptions): OutlineDatum[] {
  const dependent = dependentLayers(o.meta);
  const data: OutlineDatum[] = [];
  const seen = new Set<bigint>();
  // Opened first, so the artifact under the pointer draws as opened once it is opened.
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
    // With no roster, a served shape is smoothed as a hull.
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
  return data.sort((x, y) => x.rung - y.rung);
}

/**
 * The most label candidates offered to placement, so a whole level of a large hierarchy does not
 * make a settle a pass over tens of thousands of artifacts.
 */
export const LABEL_CANDIDATE_CEILING = 4_096;

/**
 * How many artifacts are offered to placement: every drawn one, up to
 * {@link LABEL_CANDIDATE_CEILING}. The spatial hash decides how many fit. A cap from the window
 * size would pick the largest artifacts, which crowd the same pixels, and drop small ones that fit
 * in the gaps.
 */
export function labelBudget(drawn: number): number {
  return Math.min(Math.max(0, drawn), LABEL_CANDIDATE_CEILING);
}

export type LabelText = {
  artifact: Artifact;
  /** The name as drawn, on one line. */
  line: string;
  countText: string;
  size: number;
  topic: string | null;
};

/** A name's character width as a fraction of the font size, for the placement box. */
const NAME_EM = 0.58;
/** The count's size relative to the name's, and the gap between the name and it. */
const COUNT_SCALE = 0.82;
const COUNT_GAP_EM = 0.35;
/** The topic line beneath the name, in pixels. */
const TOPIC_SIZE = 12;

/**
 * The artifacts named at `level`: every drawn artifact with no drawn child. An artifact with a
 * drawn child is an ancestor of something on the map and is not labelled.
 *
 * With no level this is the leaves of the served set. With one it is that level's artifacts and
 * every shallower artifact whose children are all deeper than the level, so a branch that ends
 * above the level is still named. The level compared is the served `rung`, which the server
 * computes per layer kind; a levelled layer's edges may skip a level, so a client-side depth count
 * would be wrong.
 */
export function frontier(a: ArtifactsProjection, level: number | undefined): Set<bigint> {
  const out = new Set<bigint>();
  for (const artifact of a.served) if (onFrontier(a, artifact, level)) out.add(artifact.tesseraId);
  return out;
}

/**
 * The label candidates for a served set at `zoom`: the top `budget` frontier artifacts by masked
 * count that have text to draw, each with its name, count, topic and pixel box. An artifact with
 * no text and no attached topic gets no label; its key is an identifier, not a name. Name size
 * comes from {@link labelSize} over the candidates' counts.
 *
 * Only the anchor depends on the zoom, so the list is built once per served set, level and budget,
 * and each zoom bucket scales the anchors into a copy.
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

/** {@link labelCandidates} with anchors in world units, which the caller scales. */
function namedCandidates(a: ArtifactsProjection, meta: Meta | null, level: number | undefined, budget: number): {candidates: LabelCandidate[]; byId: Map<bigint, LabelText>} {
  const placed = a.served.filter((x) => x.centroid !== null);
  // A dependent layer's artifacts (a clustering's topic labels) are drawn beneath their target's
  // name and are not candidates of their own.
  const dependent = new Set(meta?.layers.filter((l) => l.depsOn.length > 0).map((l) => l.name) ?? []);
  const front = frontier(a, level);
  const named = placed
    .filter((x) => !dependent.has(x.layer) && front.has(x.tesseraId))
    .filter((x) => artifactName(x, a.attached) !== null)
    .sort((x, y) => Number(y.maskedCount - x.maskedCount))
    .slice(0, Math.max(0, budget));
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
    // An artifact with no text takes its topic as the name; one with both draws the topic beneath.
    const name = artifactName(artifact, a.attached)!;
    const attached = a.attached.get(artifact.tesseraId) ?? null;
    const topic = attached === name ? null : attached;
    const countText = count.toLocaleString('en-GB');
    const line = labelLine(name);
    byId.set(artifact.tesseraId, {artifact, line, countText, size, topic});
    // The box placed is the name alone, as drawn at rest; the count and topic show on hover.
    const lineWidth = line.length * NAME_EM * size;
    candidates.push({
      id: artifact.tesseraId,
      x: gridToWorld(artifact.centroid![0]),
      y: gridToWorld(artifact.centroid![1]),
      width: lineWidth + 8,
      height: size * LABEL_LINE_HEIGHT,
      priority: count
    });
  }
  return {candidates, byId};
}

/** Whether an artifact's outline is its served shape or its box. */
export type OutlineSource = 'shape' | 'box';

/** A served artifact's outline in world space, as parts of rings, and which kind it is. */
export type Outline = {parts: Part[]; source: OutlineSource};

/**
 * A served artifact's outline in world space, with the served vertices unsmoothed: the parts of
 * `fetched` (the shape fetched by identifier) where given, else of the artifact's own shape, else
 * its box, else null. `source` says which, since a square shape and a box both have four corners
 * and are drawn differently.
 *
 * A part is an outer ring and its holes. A ring of fewer than three vertices has no area and is
 * dropped; a part whose outer ring is dropped goes whole. With no part left, the box answers. An
 * outline is a drawing: membership comes from the served `membership:<layer>` column.
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

/** The props `<tessera-map>` also passes, through `@tesseradb/deck/internal`. */
export type TesseraLayerInternalProps = TesseraLayerProps & {
  /** The marks' GPU buffers. Unset, the layer makes and releases its own; a host's is attached and released by the host. */
  slab?: MarkSlab | null;
};

/**
 * A deck.gl `CompositeLayer` that draws a Tessera store's marks and artifacts and density from
 * counts by cell. From bottom to top it draws the hovered and the opened artifact's outline, the
 * density from `densityCounts`, the marks, the artifacts' names at their centroids (with the hovered one's
 * count), the selected region and the picked mark's ring. The props are {@link TesseraLayerProps}.
 *
 * The layer draws in the 512-unit world square (`WORLD_SIZE`) for an `OrthographicView` with
 * `flipY: true`, and {@link viewInputOf} turns that view's camera into what `store.setView` takes.
 * It has no adapter for a geographic `MapView` or a MapLibre host.
 *
 * With `store` set, the layer subscribes to it and redraws on each change. The layer does not
 * fetch. Every served mark is drawn, with no budget, cap or filter applied in the layer, and a
 * value the colouring cannot resolve draws grey.
 *
 * Only an artifact at the level drawn (`clusterLevel`) is outlined. On a layer that declares a
 * shape, the outline appears once `store.needShape` has fetched the shape. On a layer that declares
 * none, the outline is the artifact's box, drawn as a thin unfilled rectangle.
 *
 * Hover and pick are the host's: pass deck's pick info to {@link resolvePick}, which returns the
 * mark or the artifact name under the pointer. Outlines, the wash and the selected region are not
 * pickable.
 *
 * The layer makes its mark buffers and colour lookup texture on its deck's device, and releases
 * them when deck finalises it.
 */
export class TesseraLayer extends CompositeLayer<TesseraLayerInternalProps> {
  /** @internal */
  static override layerName = 'TesseraLayer';
  /** @internal */
  static override defaultProps = {
    store: null,
    marks: null,
    depth: 0,
    artifacts: null,
    meta: null,
    legend: null,
    status: 'idle',
    slab: null,
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
    highlighting: false,
    points: true,
    radius: null,
    pointOpacity: null,
    density: 'smooth',
    densityCounts: null,
    densityCounter: null,
    densityResolution: DEFAULT_DENSITY_CELL_PX,
    densityColours: null,
    densityStrength: 1,
    densityScale: DEFAULT_DENSITY_SCALE,
    colouring: DEFAULT_COLOURING,
    sizing: DEFAULT_SIZING,
    pickable: true,
    onDrawn: null,
    onTimings: null
  };

  /** @internal */
  declare state: {
    tick: number;
    unsubscribe: (() => void) | null;
    subscribed: Store | null;
    zoomBucket: number;
    ownSlab: MarkSlab | null;
    ownLut: LookupTexture | null;
    wash: WashState;
    ownCounter: DensityCounter | null;
    /** The store the layer's own counter is over. */
    ownCounted: Store | null;
    /** The hexagons' colour value, kept while the scale and its top count stand, since deck re-aggregates on a new one. */
    hexColour: {scale: DensityScale; max: number; value: (cells: DensityCell[]) => number} | null;
  };

  /** @internal */
  override initializeState(): void {
    this.state = {tick: 0, unsubscribe: null, subscribed: null, zoomBucket: NaN, ownSlab: null, ownLut: null, wash: {built: null, pending: null, timer: null}, ownCounter: null, ownCounted: null, hexColour: null};
    this.follow(this.props.store ?? null);
  }

  /**
   * Labels are placed in screen space relative to their centroids, so a pan leaves them and a
   * zoom re-places them. A viewport change rebuilds the layer only when the zoom crosses a bucket.
   *
   * @internal
   */
  override shouldUpdateState(params: UpdateParameters<this>): boolean {
    if (params.changeFlags.viewportChanged) this.lookOwnCounter();
    if (super.shouldUpdateState(params)) return true;
    if (!params.changeFlags.viewportChanged) return false;
    const bucket = Math.round((params.context.viewport?.zoom ?? 0) * LABEL_ZOOM_STEP);
    return bucket !== this.state.zoomBucket;
  }

  /** @internal */
  override updateState(params: UpdateParameters<this>): void {
    super.updateState(params);
    if (params.changeFlags.propsChanged && (this.props.store ?? null) !== this.state.subscribed) {
      this.follow(this.props.store ?? null);
    }
    if (this.props.slab) this.release({slab: true, lut: false});
    this.keepOwnCounter();
  }

  /**
   * Keep the layer's own density counter: one over `store` while neither `densityCounts` nor
   * `densityCounter` is given, told what to draw on every update.
   */
  private keepOwnCounter(): void {
    const store = this.props.store ?? null;
    const wanted = store !== null && !this.props.densityCounts && !this.props.densityCounter;
    let own = this.state.ownCounter;
    if (own && (!wanted || this.state.ownCounted !== store)) {
      own.dispose();
      own = this.state.ownCounter = null;
    }
    if (!wanted) return;
    if (!own) {
      own = this.state.ownCounter = new DensityCounter(store, () => this.redrawLater());
      this.state.ownCounted = store;
      this.lookOwnCounter();
    }
    own.set({on: (this.props.density ?? 'smooth') !== 'none', cellPx: this.props.densityResolution ?? DEFAULT_DENSITY_CELL_PX});
  }

  /** Tell the layer's own counter where the viewport is. */
  private lookOwnCounter(): void {
    const viewport = this.context?.viewport;
    if (!this.state?.ownCounter || !viewport) return;
    const [x, y] = viewport.unproject([viewport.width / 2, viewport.height / 2]) as [number, number];
    this.state.ownCounter.look({target: [x, y], zoom: viewport.zoom, width: viewport.width, height: viewport.height});
  }

  /** The counts density is drawn from: `densityCounts`, else the host's counter's, else the layer's own. */
  private densityCounts(): DensityCounts | null {
    return this.props.densityCounts ?? (this.props.densityCounter ?? this.state.ownCounter)?.counts() ?? null;
  }

  /** @internal */
  override finalizeState(context: Parameters<CompositeLayer['finalizeState']>[0]): void {
    super.finalizeState(context);
    this.state.unsubscribe?.();
    this.state.unsubscribe = null;
    this.state.subscribed = null;
    this.release({slab: true, lut: true});
    if (this.state.wash.timer !== null) clearTimeout(this.state.wash.timer);
    this.state.wash = {built: null, pending: null, timer: null};
    this.state.ownCounter?.dispose();
    this.state.ownCounter = null;
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
        depth: p.depth || view.depth,
        artifacts: p.artifacts ?? s.get('artifacts'),
        meta: p.meta ?? s.get('meta'),
        legend: p.legend ?? s.get('legend'),
        status: p.status && p.status !== 'idle' ? p.status : s.get('status').status
      };
    }
    return {
      marks: p.marks ?? null,
      depth: p.depth ?? 0,
      artifacts: p.artifacts ?? null,
      meta: p.meta ?? null,
      legend: p.legend ?? null,
      status: p.status ?? 'idle'
    };
  }

  /** The host's slab, else the layer's own, made on the layer's device at first use. */
  private slab(): MarkSlab {
    if (this.props.slab) return this.props.slab;
    if (!this.state.ownSlab) {
      this.state.ownSlab = new MarkSlab();
      if (this.context.device) this.state.ownSlab.attach(this.context.device);
    }
    return this.state.ownSlab;
  }

  /** The layer's lookup texture, made on the layer's device at first use. */
  private lut(): LookupTexture {
    if (!this.state.ownLut) {
      this.state.ownLut = new LookupTexture();
      if (this.context.device) this.state.ownLut.attach(this.context.device);
    }
    return this.state.ownLut;
  }

  /** Free the GPU resources the layer made; a host's slab is left alone. */
  private release(which: {slab: boolean; lut: boolean}): void {
    if (which.slab) {
      this.state.ownSlab?.clear();
      this.state.ownSlab = null;
    }
    if (which.lut) {
      this.state.ownLut?.destroy();
      this.state.ownLut = null;
    }
  }

  /** @internal */
  override renderLayers(): LayersList {
    const started = performance.now();
    const timings: LayerTimings = {slabMs: 0, densityMs: 0, lutMs: 0, outlinesMs: 0, labelsMs: 0, layersMs: 0, lutWrites: 0, outlines: 0, outlinesDrawn: 0, labels: 0, markRadius: 0, markAlpha: 0, markCount: 0};
    this.state.zoomBucket = Math.round((this.context.viewport?.zoom ?? 0) * LABEL_ZOOM_STEP);
    const layers = this.buildLayers(timings);
    timings.layersMs = performance.now() - started;
    timings.lutWrites = this.lut().writes;
    this.props.onTimings?.(timings);
    return layers;
  }

  private buildLayers(timings: LayerTimings): LayersList {
    const r = this.resolved();
    const slab = this.slab();
    const layers: (Layer | null)[] = [];

    // The key covers the palette, level and highlight; `update` compares the table and colour map
    // itself. No texel depends on the served set's version, so it is not in the key.
    const lut = this.lut();
    const lutStarted = performance.now();
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

    // A refusal draws no marks but keeps the slab, whose bands answer the last view that
    // succeeded, so recovery does not rewrite them.
    if (r.status === 'refused' || !r.marks || r.marks.bands.length === 0 && r.marks.standIn.length === 0 && !r.marks.count.exact) {
      if (!r.marks) slab.clear();
      this.props.onDrawn?.(0, 0);
      // Every sublayer, empty, so the shader programs link while the first response is awaited.
      const density = r.status === 'refused' ? [this.washLayer(null)] : this.densityLayers(timings);
      return [...this.outlineLayers(r, timings), ...density, ...this.warmMarksLayers(), ...this.labelLayers(r, timings), ...this.selectionLayers()];
    }

    // With the marks off the slab is left as it is, and catches up when they are drawn again.
    if (this.props.points === false) {
      this.props.onDrawn?.(0, 0);
      return [...this.outlineLayers(r, timings), ...this.densityLayers(timings), ...this.warmMarksLayers(), ...this.labelLayers(r, timings), ...this.selectionLayers()];
    }

    // The slab's colour attribute holds the column colouring. Cluster colour is the lookup
    // texture, so switching to it leaves the attribute holding the last column encoding.
    const colourBy = r.legend?.colourBy ?? null;
    const clusterLayer = clusterLayerOf(colourBy);
    let column = heldColumnEncoding.get(slab) ?? {encoding: {kind: 'uniform'} as Encoding, colourBy: null};
    if (!clusterLayer) {
      column = {encoding: encodingOf(r.meta, r.legend, this.props.colouring ?? DEFAULT_COLOURING), colourBy};
      heldColumnEncoding.set(slab, column);
    }
    const encoding = column.encoding;
    const encodingKey = encodingSignature(encoding);
    // Marks carry the first layer's ordinals under any colouring, so switching to cluster colour
    // flips a uniform.
    const membershipLayer = clusterLayer ?? r.artifacts?.layers[0] ?? '';
    const useLut = clusterLayer !== null && lut.gpu !== null;
    const highlighting = this.props.highlighting ?? false;
    const sizing = drawnSizing(this.props.sizing ?? DEFAULT_SIZING);
    const size = sizeEncodingOf(r.meta, r.legend, sizing.scale);
    const sized = size.kind !== 'none';
    const slabStarted = performance.now();
    slab.sync(r.marks.bands, r.depth, encoding, column.colourBy, membershipLayer, size);
    timings.slabMs = performance.now() - slabStarted;

    if (!checkedMarks.has(r.marks)) {
      checkedMarks.add(r.marks);
      // Every exact band the frame draws must have a slab slot, or the picture would be thinner
      // than what was served.
      for (const band of r.marks.bands) {
        if (!slab.holds(band)) {
          throw new Error(`TesseraLayer: exact band ${band.prefix} at depth ${band.depth} is drawn but has no slab slot.`);
        }
      }
    }

    layers.push(...this.densityLayers(timings));

    const standIn = this.standInBuffers(r.marks, column.colourBy, membershipLayer, sized ? size.column : null);
    const style = markStyle(slab.drawn + standIn.count, this.context.viewport?.zoom ?? 0, this.props.radius ?? null, this.props.pointOpacity ?? null);
    // Under sizing the layer's radius is the largest any mark draws at, and each mark's own is a
    // fraction of it. The edges are feathered, and the shader keeps a small mark's edge hard.
    const radius = sized ? sizingRadius(sizing) : style.radius;
    const antialiasing = sized ? true : style.antialiasing;
    const sizeRange = sized ? sizing : null;
    const opacity = deckOpacity(style.alpha);
    const dullColour = DULL_COLOUR[this.props.scheme ?? 'dark'];
    timings.markRadius = sized ? sizing.max : style.radius;
    timings.markAlpha = style.alpha;
    timings.markCount = slab.drawn + standIn.count;

    // One layer per retained slab partition, addressed by slot, toggled by `visible`. deck
    // destroys an omitted layer and re-uploads its buffers when it returns.
    const passes: HighlightPass[] = highlighting ? ['dull', 'glow', 'lit'] : ['all'];
    const partitions = slab.layers();
    // Only the slab's warm layer: the stand-in layer is pushed after the partitions', and two
    // layers may not share the id `marks-standin`.
    if (partitions.length === 0) layers.push(...this.warmMarksLayers(false));
    // Stand-ins draw like any other mark, at the same alpha and through the same texture: each is
    // a served point with its served ordinal. No count is shown for a non-exact tile.
    const colours = this.standInColours(standIn, encoding, encodingKey);
    const sizes = this.standInSizes(standIn, size);
    if (colours.length !== standIn.count * 4) {
      throw new Error(
        `colour buffer covers ${colours.length / 4} of ${standIn.count} stand-in marks. Colour is presentation and must never decide what is drawn.`
      );
    }
    // Under a highlight every mark layer is drawn three times, dulled, glowing and lit; see
    // `HighlightPass`.
    for (const pass of passes) {
      for (const held of partitions) {
        layers.push(
          new MarksLayer(
            this.getSubLayerProps({id: pass === 'all' ? `marks-p${held.slot}` : `marks-p${held.slot}-${pass}`}),
            {
              visible: held.active && held.draw.length > 0,
              data: {
                length: held.draw.length,
                attributes: held.draw.gpu
                  ? gpuAttributes(held.draw.gpu)
                  : {
                      getPosition: binary(held.draw.positions, 2),
                      getFillColor: binary(held.draw.colours, 4, true),
                      getOrdinal: binary(held.draw.ordinals, 1),
                      getHighlight: binary(held.draw.highlights, 1),
                      getSize: binary(held.draw.sizes, 1)
                    }
              },
              tesseraIds: held.draw.ids,
              tesseraPositions: held.draw.positions,
              useLut,
              highlighting,
              highlightPass: pass,
              dullColour,
              lutTexture: lut.gpu,
              sizing: sizeRange,
              radiusUnits: 'pixels' as const,
              getRadius: radius,
              radiusMinPixels: 1,
              antialiasing,
              opacity,
              // Each mark is drawn by one of the dull and lit passes, which pick; the glow does not.
              pickable: this.props.pickable && held.active && pass !== 'glow',
              parameters: {depthCompare: 'always' as const}
            } as never
          )
        );
      }
      layers.push(
        new MarksLayer(
          this.getSubLayerProps({id: pass === 'all' ? 'marks-standin' : `marks-standin-${pass}`}),
          {
            visible: standIn.count > 0,
            data: {
              length: standIn.count,
              attributes: {
                getPosition: binary(standIn.positions, 2),
                getFillColor: binary(colours, 4, true),
                getOrdinal: binary(standIn.ordinals, 1),
                getHighlight: binary(standIn.highlights, 1),
                getSize: binary(sizes, 1)
              }
            },
            tesseraIds: standIn.ids,
            tesseraPositions: standIn.positions,
            useLut,
            highlighting,
            highlightPass: pass,
            dullColour,
            lutTexture: lut.gpu,
            sizing: sizeRange,
            radiusUnits: 'pixels' as const,
            getRadius: radius,
            radiusMinPixels: 1,
            antialiasing,
            opacity,
            pickable: this.props.pickable && pass !== 'glow',
            parameters: {depthCompare: 'always' as const}
          } as never
        )
      );
    }

    this.props.onDrawn?.(slab.drawn, standIn.count);
    // The outlines go under everything: the marks show through the hovered one's faint fill.
    layers.unshift(...this.outlineLayers(r, timings));
    layers.push(...this.labelLayers(r, timings), ...this.selectionLayers());
    return layers;
  }

  /**
   * The mark layers with nothing in them, the first partition's and the stand-ins', so their
   * programs link at the first paint instead of when the first marks arrive. luma links
   * synchronously, at 130 to 190 ms per program on the main thread. A layer that later fills keeps
   * its id, so deck updates it.
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

  private standInBuffers(marks: MarksProjection, colourBy: string | null, layer: string, sizeBy: string | null): StandInBuffers {
    const key = `${colourBy ?? ''}|${layer}|${sizeBy ?? ''}`;
    const held = heldStandIn.get(marks.standIn);
    if (held && held.key === key) return held.buffers;
    const buffers = materialiseStandIn(marks.standIn, new Set([colourBy, sizeBy].filter((c): c is string => c !== null)), layer);
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

  private standInSizes(standIn: StandInBuffers, size: SizeEncoding): Float32Array {
    const key = sizeSignature(size);
    const held = heldStandInSizes.get(standIn);
    if (held && held.key === key) return held.sizes;
    const sizes = buildSizeAttribute(standIn.count, standIn.scalars, size);
    heldStandInSizes.set(standIn, {key, sizes});
    return sizes;
  }

  /**
   * Whether the labels sit on plates: while density is drawn as hexagons or a grid, or as a smooth
   * wash in a ramp, whose colours fill the ground under the names.
   */
  private platesLabels(): boolean {
    const mode = this.props.density ?? 'smooth';
    return mode === 'hex' || mode === 'grid' || (mode === 'smooth' && this.densityColours(mode) !== 'warm-grey');
  }

  /** The density colours drawn: the prop, else warm grey for a wash under the marks, else Viridis. */
  private densityColours(mode: DensityMode): DensityColours {
    return this.props.densityColours ?? (mode === 'smooth' && this.props.points !== false ? 'warm-grey' : 'viridis');
  }

  /**
   * The density layers for `density`, all built from the counts by cell ({@link densityCounts}),
   * never from the marks. The wash's layer is always present, hidden unless `smooth` is drawn, so
   * its program links at the first paint.
   *
   * Every mode places counts on `densityScale` up to the largest count among the cells it draws
   * ({@link drawnCells}). The grid is one image, a square per cell in the colour of its count's
   * position. The hexagons colour each bin by the position of the mean count of the cells whose
   * centres fall in it, so a bin that happens to hold two cell centres does not read as twice as
   * dense. The contours are drawn at the counts {@link contourThresholds} picks, in the label ink.
   * The hexagons and contours come from `@deck.gl/aggregation-layers`, loaded the first time one
   * is asked for ({@link loadAggregationLayers}); nothing of theirs is drawn until it has loaded.
   */
  private densityLayers(timings: {densityMs: number}): Layer[] {
    const started = performance.now();
    const mode = this.props.density ?? 'smooth';
    const scheme = this.props.scheme ?? 'dark';
    const colours = this.densityColours(mode);
    const strength = Math.min(1, Math.max(0, this.props.densityStrength ?? 1));
    const scale = this.props.densityScale ?? DEFAULT_DENSITY_SCALE;
    const counts = mode === 'none' ? null : this.densityCounts();
    const layers: Layer[] = [this.washLayer(mode === 'smooth' || mode === 'grid' ? counts : null, mode === 'grid' ? 'grid' : 'smooth', colours, strength, scale)];
    if ((mode === 'hex' || mode === 'contours') && counts) {
      const aggregated = drawnCells(counts, mode);
      const span = WORLD_SIZE / 2 ** aggregated.depth;
      const stops = densityStops(colours, scheme);
      const common = {opacity: strength, pickable: false, parameters: {depthCompare: 'always' as const}};
      if (!aggregation) {
        void loadAggregationLayers().then(() => this.redrawLater());
      } else if (mode === 'contours') {
        const thresholds = contourThresholds(aggregated.cells, scale);
        const ink = INK[scheme];
        layers.push(
          new aggregation.ContourLayer(this.getSubLayerProps({id: 'density-contours'}), {
            ...common,
            data: aggregated.cells,
            getPosition: (d: DensityCell) => d.position,
            gpuAggregation: false,
            cellSize: span,
            getWeight: (d: DensityCell) => d.count,
            aggregation: 'SUM',
            contours: thresholds.map((threshold, i) => ({
              threshold,
              color: [ink[0], ink[1], ink[2], Math.round(90 + (140 * (i + 1)) / thresholds.length)] as [number, number, number, number],
              strokeWidth: i === thresholds.length - 1 ? 1.5 : 1
            }))
          } as never)
        );
      } else {
        const max = maxCount(aggregated.cells);
        let held = this.state.hexColour;
        if (!held || held.scale !== scale || held.max !== max) {
          held = {scale, max, value: (cells) => densityPosition(cells.reduce((n, c) => n + c.count, 0) / cells.length, max, scale)};
          this.state.hexColour = held;
        }
        layers.push(
          new aggregation.HexagonLayer(this.getSubLayerProps({id: 'density-hex'}), {
            ...common,
            data: aggregated.cells,
            getPosition: (d: DensityCell) => d.position,
            gpuAggregation: false,
            getColorValue: held.value,
            colorScaleType: 'quantize',
            colorDomain: [0, 1],
            colorRange: Array.from({length: DENSITY_STEPS}, (_, i) => [...rampAtStops(stops, i / (DENSITY_STEPS - 1))] as [number, number, number]),
            coverage: DENSITY_COVERAGE,
            extruded: false,
            radius: span
          } as never)
        );
      }
    }
    timings.densityMs = performance.now() - started;
    return layers;
  }

  /** Ask for a redraw of the layer that is current for this id, which may no longer be this instance. */
  private redrawLater(): void {
    const current = (this.getCurrentLayer?.() as TesseraLayer | null) ?? this;
    // A discarded instance has no manager to ask; the next paint reads the state anyway.
    if (!current.lifecycle || /Discarded|Finalized/.test(String(current.lifecycle))) return;
    current.setNeedsUpdate();
    current.setNeedsRedraw();
  }

  /**
   * The smooth wash or the grid from `counts`, as one image built once per counts object and off the
   * paint path, since a viewport of fine cells takes tens of milliseconds to bin and filter. The
   * previous image, if any, draws until the build has run. The wash is filtered linearly so the
   * cells do not show, and the grid by nearest texel so they do. `counts` null draws the empty wash
   * so its program links at the first paint.
   */
  private washLayer(counts: DensityCounts | null, kind: 'smooth' | 'grid' = 'smooth', colours: DensityColours = 'warm-grey', strength = 1, scale: DensityScale = DEFAULT_DENSITY_SCALE): Layer {
    let image: ImageData | null = null;
    let bounds: [number, number, number, number] = [0, 0, 1, 1];
    if (counts) {
      const want: WashKey = {counts, kind, scheme: this.props.scheme ?? 'dark', colours, scale};
      const wash = this.state.wash;
      if (!sameWash(wash.built, want) && !sameWash(wash.pending, want)) {
        wash.pending = want;
        const build = () => {
          wash.timer = null;
          wash.pending = null;
          let built;
          if (want.kind === 'grid') built = gridImage(want.counts, densityStops(want.colours, want.scheme), DENSITY_STEPS, want.scale);
          else {
            const binned = binDensity(want.counts, want.scale);
            built = binned ? filterDensity(binned, want.counts.depth, densityPaint(want.colours, want.scheme)) : null;
          }
          wash.built = {
            ...want,
            image: built && typeof ImageData !== 'undefined' ? new ImageData(built.data, built.width, built.height) : null,
            bounds: built ? built.bounds : [0, 0, 1, 1]
          };
          this.redrawLater();
        };
        if (typeof setTimeout !== 'undefined') {
          if (wash.timer !== null) clearTimeout(wash.timer);
          wash.timer = setTimeout(build, 0);
        } else build();
      }
      image = wash.built?.image ?? null;
      bounds = wash.built?.bounds ?? bounds;
    }
    const [x0, y0, x1, y1] = bounds;
    const grid = kind === 'grid';
    return new BitmapLayer(
      this.getSubLayerProps({id: grid ? 'density-grid' : 'wash'}),
      {
        visible: image !== null,
        image: image ?? EMPTY_IMAGE,
        // `[left, bottom, right, top]`: row 0 of the image is the lowest cell row, which is the
        // smaller world y, and the view is y-down, so `top` is `y0`.
        bounds: [x0, y1, x1, y0],
        opacity: strength,
        pickable: false,
        textureParameters: grid ? {minFilter: 'nearest', magFilter: 'nearest'} : {minFilter: 'linear', magFilter: 'linear'},
        parameters: {depthCompare: 'always' as const}
      } as never
    );
  }

  /**
   * The hovered and opened artifacts' outlines ({@link focusOutlines}). A derived shape is computed
   * per principal by the server; nothing is contoured from held marks. The layer is not pickable:
   * hover and click over a contour resolve against {@link contourShapes}.
   *
   * The memo is keyed on the fetched shapes as well as the served set, since a shape arriving by
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
    // Present from the first paint, empty, so its program links early.
    return [
      new PolygonLayer(
        this.getSubLayerProps({id: 'outlines'}),
        {
          visible: data.length > 0,
          data,
          getPolygon: (d: OutlineDatum) => d.polygon,
          // A box draws unfilled through its `fill` alpha of 0.
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
   * Names at each artifact's centroid, placed by priority into a spatial hash, with a leader line
   * where a label moved. The hovered artifact's name also shows its count after it and, where a
   * dependent artifact's text is attached, that text beneath. Text is deck's `TextLayer` with
   * `characterSet: 'auto'` and an SDF halo.
   */
  private labelLayers(r: Resolved, timings: {labelsMs: number; labels: number}): Layer[] {
    const a = this.props.labels ? r.artifacts : null;
    const viewport = this.context.viewport;
    const started = performance.now();
    const zoom = viewport?.zoom ?? 0;
    const bucket = Math.round(zoom * LABEL_ZOOM_STEP);
    const budget = a ? labelBudget(a.served.length) : 0;
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
        const {artifact, line, countText, size, topic} = byId.get(p.id)!;
        const position = gridToWorldXY(artifact.centroid!);
        const ordinal = a.table.ordinalOf(artifact.layer, artifact.tesseraId);
        const colour = a.colours.get(ordinal) ?? NEUTRAL;
        // The name centred on the anchor; the count starts where the name ends, and the topic
        // sits beneath, so neither overlaps it.
        const seam = p.dx + (line.length * NAME_EM * size) / 2;
        const baseline = p.dy;
        data.push({id: artifact.tesseraId, position, text: line, size, offset: [p.dx, baseline], colour, kind: 'name', anchor: 'middle'});
        data.push({id: artifact.tesseraId, position, text: countText, size: size * COUNT_SCALE, offset: [seam + size * COUNT_GAP_EM, baseline + size * 0.08], colour, kind: 'count', anchor: 'start'});
        if (topic) data.push({id: artifact.tesseraId, position, text: topic, size: TOPIC_SIZE, offset: [p.dx, baseline + size * 0.78 + 3], colour, kind: 'topic', anchor: 'middle'});
        if (p.leader) leaders.push({from: position, to: [position[0] + p.dx / scale, position[1] + p.dy / scale]});
      }
      held = {key, data, leaders, placed};
      heldLabels.set(a.served, held);
    }
    const data = held?.data ?? NO_LABELS;
    const leaders = held?.leaders ?? NO_LEADERS;
    timings.labelsMs = performance.now() - started;
    // Labels placed, not text rows: a label is its name, its count and its topic.
    timings.labels = held?.placed ?? 0;
    // Present from the first paint, empty, so their programs link early.
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
    // Over density in saturated colours a halo in the ground colour reads as an outline round each
    // glyph, so the names sit on plates of the ground colour there instead. The plates are drawn as
    // layers of their own beneath all the text, so one row's plate does not cover another's glyphs.
    const plated = this.platesLabels();
    const plateProps = {background: true, getBackgroundColor: HALO[scheme], backgroundPadding: [6, 4], backgroundBorderRadius: 3, getColor: [0, 0, 0, 0], outlineWidth: 0, pickable: false};
    const own = plated ? {outlineWidth: 0} : {};
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
          fontFamily: "'Instrument Sans', system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif",
          // The halo is drawn from the distance field. deck's default buffer of 4 clips the field
          // to about a third of a pixel of outline on a 12 px name; see HALO_RADIUS.
          fontSettings: {sdf: true, buffer: HALO_BUFFER, radius: HALO_RADIUS, cutoff: 0.25},
          outlineWidth: HALO_OUTLINE_WIDTH,
          outlineColor: HALO[scheme],
          characterSet: rows.length > 0 ? 'auto' : WARM_GLYPHS,
          pickable: this.props.pickable && kind === 'name',
          artifactIds: rows.map((d) => d.id),
          parameters: {depthCompare: 'always' as const},
          updateTriggers: {getPixelOffset: key, getSize: key, getColor: scheme, getBackgroundColor: scheme},
          ...extra
        } as never
      );
    const kinds = [
      {id: 'labels', kind: 'name', style: {fontWeight: 600}},
      {id: 'label-counts', kind: 'count', style: {fontWeight: 400}},
      {id: 'label-topics', kind: 'topic', style: {fontWeight: 400, fontStyle: 'italic'}}
    ] as const;
    // A name shows at rest; its count and topic only while its artifact is hovered.
    const hovered = this.props.hoveredArtifact ?? null;
    const rowsOf = (kind: LabelDatum['kind']) => data.filter((d) => d.kind === kind && (kind === 'name' || d.id === hovered));
    if (plated) for (const k of kinds) layers.push(text(`${k.id}-plates`, k.kind, rowsOf(k.kind), {...k.style, ...plateProps}));
    for (const k of kinds) layers.push(text(k.id, k.kind, rowsOf(k.kind), {...k.style, ...own}));
    return layers;
  }

  /** The picked mark's marker, and the selected region as the shape drawn. */
  private selectionLayers(): Layer[] {
    const layers: Layer[] = [];
    const box = this.props.drag ?? this.props.region ?? null;
    const polygon = this.props.dragPolygon ?? this.props.regionPolygon ?? null;
    const shape: [number, number][] | null =
      polygon && polygon.length >= 2 ? polygon : box ? [[box[0], box[1]], [box[2], box[1]], [box[2], box[3]], [box[0], box[3]]] : null;
    const live = this.props.drag != null || this.props.dragPolygon != null;
    const scheme = this.props.scheme ?? 'dark';
    const accent = ACCENT[scheme];
    // Present from the first paint, empty, so their programs link early.
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
