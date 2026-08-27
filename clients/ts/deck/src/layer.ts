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
  type Store,
  type TilesProjection
} from '@tesseradb/client';
import {materialiseStandIn, type StandInBuffers} from './assemble.js';
import {buildColourAttribute, type Encoding} from './colour.js';
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
 * and pick are the host's: the sublayers carry `tesseraIds` and `artifactIds`, and `resolvePick`
 * turns deck's pick info into a mark, an artifact, a miss or a broken pick.
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
 * The drawing, in order (§5.10): the hovered and the opened artifact's served `hull` or `box`,
 * every other served shape in the data at zero alpha so it still answers a pick; the single-hue
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
  /** The served shapes the outline layer holds, and how many of them actually draw. */
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
/** The outline polygons, once per served set, opened artifact and zoom bucket. */
const heldOutlines = new WeakMap<object, {key: string; data: OutlineDatum[]}>();
/** The label placement, once per served set and zoom bucket. */
const heldLabels = new WeakMap<object, {key: string; data: LabelDatum[]; leaders: LeaderDatum[]; placed: number}>();

export type OutlineDatum = {
  id: bigint;
  polygon: [number, number][];
  colour: Rgba;
  opened: boolean;
  hovered: boolean;
  /** The artifact's depth in the served tree. */
  depth: number;
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

/** The hovered outline: a fuller fill and a firmer line than the hairline, less than the opened one's. */
const HOVER_FILL: Record<'light' | 'dark', number> = {light: 26, dark: 33};
const HOVER_LINE = 150;
/** The opened outline: the boards' 0.16 fill and a strong line. */
const OPENED_FILL = 41;
const OPENED_LINE = 200;

export type OutlineOptions = {
  opened: bigint | null;
  hovered: bigint | null;
  level: number | undefined;
  scheme: 'light' | 'dark';
};

/**
 * The served outlines and how each draws (§5.10). **A hull is drawn only for the hovered and the
 * opened artifact** — on every layer, nested included (the owner's review, 2026-08-26). At rest
 * the map is colour and names.
 *
 * The rule already held for a flat layer, whose hulls overlap into a mesh, and the argument is the
 * same one level up: a response carries a frontier **and its ancestors**, so drawing every served
 * hull gave each region its own shape plus its parent's plus its grandparent's, translucent fills
 * stacking into a murky wash with no cue that the big shape contained the small ones. Exact colour
 * already says where a cluster is and how far it reaches, per principal, so the contours were
 * paying for a thing already drawn.
 *
 * Every other artifact stays in the data at zero alpha, which is what answers a pick — the flat
 * path's own arrangement, reused rather than forked. Parents are ordered first so an opened child
 * sits over an opened parent.
 */
export function outlineData(a: ArtifactsProjection, o: OutlineOptions): OutlineDatum[] {
  const data: OutlineDatum[] = [];
  const depths = servedDepths(a);
  const ordered = [...a.served].sort((x, y) => (depths.get(x.tesseraId) ?? 0) - (depths.get(y.tesseraId) ?? 0));
  for (const artifact of ordered) {
    const depth = depths.get(artifact.tesseraId) ?? 0;
    if (o.level !== undefined && depth > o.level) continue;
    const polygon = outlineOf(artifact);
    if (!polygon) continue;
    const ordinal = a.table.ordinalOf(artifact.layer, artifact.tesseraId);
    const opened = artifact.tesseraId === o.opened;
    const hovered = !opened && artifact.tesseraId === o.hovered;
    const fill = opened ? OPENED_FILL : hovered ? HOVER_FILL[o.scheme] : 0;
    const line = opened ? OPENED_LINE : hovered ? HOVER_LINE : 0;
    const width = opened ? 1.2 : hovered ? 1 : 0.8;
    data.push({id: artifact.tesseraId, polygon, colour: a.colours.get(ordinal) ?? NEUTRAL, opened, hovered, depth, fill, line, width});
  }
  return data;
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
 * owner's review, 2026-08-26) — computed here from `parentId` over the served set, which is what
 * `lineage` already holds, so nothing new is asked of the wire.
 *
 * With no chosen level this is the cut's leaves. With one it is that level's artifacts **and**
 * every shallower artifact whose own children the level cut away, which the plain `depth === level`
 * test that stood here dropped — a branch that stops above the chosen level went unnamed.
 */
export function frontier(a: ArtifactsProjection, level: number | undefined, depths: Map<bigint, number> = servedDepths(a)): Set<bigint> {
  const drawn = (id: bigint) => level === undefined || (depths.get(id) ?? 0) <= level;
  const out = new Set<bigint>();
  for (const artifact of a.served) {
    if (!drawn(artifact.tesseraId)) continue;
    const children = a.lineage.childrenOf.get(artifact.tesseraId) ?? [];
    if (!children.some((c) => drawn(c.tesseraId))) out.add(artifact.tesseraId);
  }
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
 */
export function labelCandidates(a: ArtifactsProjection, meta: Meta | null, level: number | undefined, zoom: number, budget: number): {candidates: LabelCandidate[]; byId: Map<bigint, LabelText>} {
  const placed = a.served.filter((x) => x.centroid !== null);
  // A dependent layer's artifacts — a clustering's topic labels — draw their text beneath the
  // name of whatever they sit on, italic and small, and are placed with it: they are not
  // candidates of their own (§5.10, D13).
  const dependent = new Set(meta?.layers.filter((l) => l.depsOn.length > 0).map((l) => l.name) ?? []);
  const topicOf = attachedTopics(a, meta);
  const depths = servedDepths(a);
  const front = frontier(a, level, depths);
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
  const scale = 2 ** zoom; // pixels per world unit
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
      x: gridToWorld(artifact.centroid![0]) * scale,
      y: gridToWorld(artifact.centroid![1]) * scale,
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

/**
 * A served artifact's outline in world space: its hull, else its box, else nothing — **the wire's
 * own vertices, in the wire's own order, and nothing else**.
 *
 * The hull was smoothed here by three rounds of Chaikin's corner cutting, on the reading that
 * every vertex it produced stayed inside the hull's convex extent. That held only while the served
 * hull *was* convex. It is now a concave shape that follows the cluster's arms (annotations §4.2),
 * and Chaikin cuts a **reflex** corner outward: the triangle it removes at a reflex vertex lies
 * outside the polygon, so a smoothed contour bulged past the served shape at every concavity, by
 * up to a quarter of the shorter adjacent edge. It is a display matter and not a disclosure —
 * the client invents no vertex from data — but a drawn shape must not claim area the served shape
 * does not have, and every vertex the engine sends is a visible member's own position, which is
 * exactly the property smoothing threw away.
 *
 * So the smoothing is gone rather than made reflex-aware. The shape no longer needs it: its
 * corners are the members', not a convex wrap's artefacts, and at the 52–87 vertices the concave
 * path produces they are small. It also cost eight times the vertices on every served outline —
 * about 130 to 700 on one shape — and an outline is materialised for every served artifact,
 * drawn or not, because the polygon is what answers a pick.
 */
export function outlineOf(a: Artifact): [number, number][] | null {
  const w = gridToWorld;
  if (a.hull && a.hull.length >= 3) return a.hull.map(gridToWorldXY);
  if (a.box) return [[w(a.box[0]), w(a.box[1])], [w(a.box[2]), w(a.box[1])], [w(a.box[2]), w(a.box[3])], [w(a.box[0]), w(a.box[3])]];
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

/** The depth of each served artifact in the served tree: a root is 0, a child one deeper. */
export function servedDepths(a: ArtifactsProjection): Map<bigint, number> {
  const depth = new Map<bigint, number>();
  const of = (id: bigint, guard = 0): number => {
    const known = depth.get(id);
    if (known !== undefined) return known;
    const artifact = a.lineage.byId.get(id);
    const parent = artifact && artifact.parentId !== null && a.lineage.byId.has(artifact.parentId) && guard < 1024 ? of(artifact.parentId, guard + 1) + 1 : 0;
    depth.set(id, parent);
    return parent;
  };
  for (const artifact of a.served) of(artifact.tesseraId);
  return depth;
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

    // The lookup texture is rewritten whenever the table, the served set, the palette, the level
    // or the highlight moved — O(table range), never O(points) — and the device it lives on is
    // deck's. **The table's own version is in the key**: a point response names artifacts the
    // debounced channel has not served yet, and without it those ordinals kept the texture's
    // neutral until the channel's next answer bumped `version`.
    const lut = this.lut();
    const lutStarted = performance.now();
    if (this.context.device && !lut.gpu) lut.attach(this.context.device);
    const opened = this.props.openedArtifact ?? null;
    const highlight = r.artifacts && opened !== null ? r.artifacts.served.find((a) => a.tesseraId === opened) : undefined;
    const highlightOrdinal = highlight && r.artifacts ? r.artifacts.table.ordinalOf(highlight.layer, highlight.tesseraId) : NO_ORDINAL;
    if (r.artifacts) {
      lut.update(
        {artifacts: r.artifacts, level: this.props.clusterLevel, highlight: highlightOrdinal},
        `${r.artifacts.version}|${r.artifacts.table.version}|${r.artifacts.palette}|${r.artifacts.table.range}|${this.props.clusterLevel ?? ''}|${highlightOrdinal}`
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
   * The served `hull` or `box` for the hovered and the opened artifact, in its own colour, the
   * opened one strong with a faint fill; every other served shape in the data at zero alpha so it
   * still answers a pick ({@link outlineData}). Derived per principal (contracts §3.2), so a shape
   * is exact for this viewer; nothing is contoured from held marks (decision 0099).
   *
   * The shapes do not depend on the zoom, so the memo survives a zoom that re-places the labels.
   */
  private outlineLayers(r: Resolved, timings: {outlinesMs: number; outlines: number; outlinesDrawn: number}): Layer[] {
    const a = r.artifacts;
    const started = performance.now();
    const opened = this.props.openedArtifact ?? null;
    const hovered = this.props.hoveredArtifact ?? null;
    const scheme = this.props.scheme ?? 'dark';
    const key = a ? `${a.version}|${a.palette}|${opened ?? ''}|${hovered ?? ''}|${scheme}|${this.props.clusterLevel ?? ''}` : '';
    let held = a ? heldOutlines.get(a.served) : undefined;
    if (a && (!held || held.key !== key)) {
      held = {key, data: outlineData(a, {opened, hovered, level: this.props.clusterLevel, scheme})};
      heldOutlines.set(a.served, held);
    }
    const data = held?.data ?? NO_OUTLINES;
    timings.outlinesMs = performance.now() - started;
    timings.outlines = data.length;
    timings.outlinesDrawn = data.reduce((n, d) => n + (d.fill > 0 || d.line > 0 ? 1 : 0), 0);
    // The layer exists from the first paint, empty, so its program is linked before it is needed.
    return [
      new PolygonLayer(
        this.getSubLayerProps({id: 'outlines'}),
        {
          visible: data.length > 0,
          data,
          getPolygon: (d: OutlineDatum) => d.polygon,
          // Filled even at zero alpha: the fill is what answers a pick, and a flat layer's
          // outline is invisible until hovered while its hull still names the artifact under
          // the pointer (deck's picking pass reads the picking colour, never the fill's alpha).
          filled: true,
          getFillColor: (d: OutlineDatum) => [d.colour[0], d.colour[1], d.colour[2], d.fill],
          stroked: true,
          getLineColor: (d: OutlineDatum) => [d.colour[0], d.colour[1], d.colour[2], d.line],
          lineWidthUnits: 'pixels' as const,
          getLineWidth: (d: OutlineDatum) => d.width,
          lineWidthMinPixels: 0.8,
          pickable: this.props.pickable,
          artifactIds: data.map((d) => d.id),
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
