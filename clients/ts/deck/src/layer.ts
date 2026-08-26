import {CompositeLayer, type BinaryAttribute as DeckBinaryAttribute, type CompositeLayerProps, type Layer, type LayersList, type UpdateParameters} from '@deck.gl/core';
import {BitmapLayer, LineLayer, PolygonLayer, ScatterplotLayer, TextLayer} from '@deck.gl/layers';
import {
  CLUSTER_PREFIX,
  NEUTRAL,
  NO_ORDINAL,
  GRID32_PER_WORLD_UNIT as GRID32_PER_WORLD,
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
import {labelSize, placeLabels, type LabelCandidate, type PlacedLabel} from './labels.js';
import {LookupTexture} from './lut.js';
import {MarksLayer} from './marks-layer.js';
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
 * The drawing, in order (§5.10): the served `hull` or `box` as faded hairline outlines, the
 * opened one strong with a faint fill; the single-hue density wash from the exact tiles' counts,
 * filtered so the tile grid never shows (decision 0097); the marks — one `MarksLayer` per
 * retained slab partition, addressed by slot, plus the stand-ins — in their membership colour
 * through the lookup texture when colouring by cluster, else the column's colour; names and
 * counts at each artifact's `centroid`, placed by priority into a spatial hash with leader
 * lines; the picked mark; and the selected region as the shape drawn — a box or a lasso, never
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
  /** The selected region's world shape, and the live shape while it is being drawn. */
  region?: [number, number, number, number] | null;
  regionPolygon?: [number, number][] | null;
  drag?: [number, number, number, number] | null;
  dragPolygon?: [number, number][] | null;
  /** Whether the density wash is drawn under the points. */
  wash?: boolean;
  radius?: number;
  /** How many marks a paint ended up drawing, for the host's probe. */
  onDrawn?: ((drawn: number, provisional: number) => void) | null;
  /** Per-settle work, in ms — the slab sync, the wash bin, the lookup texture, the outlines, the labels, the whole layer build — for the harness. */
  onTimings?: ((t: {slabMs: number; washMs: number; lutMs: number; outlinesMs: number; labelsMs: number; layersMs: number; lutWrites: number; outlines: number; labels: number}) => void) | null;
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
const HALO: Record<'light' | 'dark', [number, number, number, number]> = {light: [247, 247, 244, 190], dark: [12, 14, 17, 180]};
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
/** The outline polygons, once per served set and opened artifact. */
const heldOutlines = new WeakMap<object, {key: string; data: OutlineDatum[]}>();
/** The label placement, once per served set and zoom bucket. */
const heldLabels = new WeakMap<object, {key: string; data: LabelDatum[]; leaders: LeaderDatum[]}>();

type OutlineDatum = {id: bigint; polygon: [number, number][]; colour: Rgba; opened: boolean};
type LabelDatum = {id: bigint; position: [number, number]; text: string; size: number; offset: [number, number]; colour: Rgba; kind: 'name' | 'count' | 'topic'};
type LeaderDatum = {from: [number, number]; to: [number, number]};

/** The faint outline's alpha per ground: the boards' 0.16 on light, 0.22 on dark. */
const HAIRLINE_ALPHA: Record<'light' | 'dark', number> = {light: 44, dark: 60};

/** What to call an artifact: its supplied text where the layer publishes any, else its key. */
export function artifactName(a: Artifact): string {
  const text = a.content[0];
  if (text !== undefined && text.length > 0) return text;
  return a.key ?? `#${a.tesseraId}`;
}

/** A served artifact's outline in world space: its hull, else its box, else nothing. */
export function outlineOf(a: Artifact): [number, number][] | null {
  const w = gridToWorld;
  if (a.hull && a.hull.length >= 3) return a.hull.map(gridToWorldXY);
  if (a.box) return [[w(a.box[0]), w(a.box[1])], [w(a.box[2]), w(a.box[1])], [w(a.box[2]), w(a.box[3])], [w(a.box[0]), w(a.box[3])]];
  return null;
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
    wash: true,
    radius: 1.6,
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
    const timings = {slabMs: 0, washMs: 0, lutMs: 0, outlinesMs: 0, labelsMs: 0, layersMs: 0, lutWrites: 0, outlines: 0, labels: 0};
    this.state.zoomBucket = Math.round((this.context.viewport?.zoom ?? 0) * LABEL_ZOOM_STEP);
    const layers = this.buildLayers(timings);
    timings.layersMs = performance.now() - started;
    timings.lutWrites = this.lut().writes;
    this.props.onTimings?.(timings);
    return layers;
  }

  private buildLayers(timings: {slabMs: number; washMs: number; lutMs: number; outlinesMs: number; labelsMs: number; outlines: number; labels: number}): LayersList {
    const r = this.resolved();
    const {slab} = this.props;
    const layers: (Layer | null)[] = [];

    // The lookup texture is rewritten whenever the served set, the palette, the level or the
    // highlight moved — O(table range), never O(points) — and the device it lives on is deck's.
    const lut = this.lut();
    const lutStarted = performance.now();
    if (this.context.device && !lut.gpu) lut.attach(this.context.device);
    const opened = this.props.openedArtifact ?? null;
    const highlight = r.artifacts && opened !== null ? r.artifacts.served.find((a) => a.tesseraId === opened) : undefined;
    const highlightOrdinal = highlight && r.artifacts ? r.artifacts.table.ordinalOf(highlight.layer, highlight.tesseraId) : NO_ORDINAL;
    if (r.artifacts) {
      lut.update(
        {artifacts: r.artifacts, level: this.props.clusterLevel, highlight: highlightOrdinal},
        `${r.artifacts.version}|${r.artifacts.palette}|${r.artifacts.table.range}|${this.props.clusterLevel ?? ''}|${highlightOrdinal}`
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

    // Layers toggle `visible`; they are never omitted — deck destroys an absent layer and re-uploads
    // everything it held when it returns. One layer per retained slab partition, addressed by slot,
    // so a depth flip is a swap and flipping back uploads nothing.
    const partitions = slab.layers();
    if (partitions.length === 0) layers.push(...this.warmMarksLayers());
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
            getRadius: this.props.radius,
            radiusMinPixels: 1,
            pickable: this.props.pickable && held.active,
            parameters: {depthCompare: 'always' as const}
          } as never
        )
      );
    }

    // The stand-ins: drawn at full alpha like any other mark. What guards the reading is the
    // number channel — no count is shown against a non-exact tile — not the alpha channel. They
    // carry no ordinal and draw neutral under cluster colour: a stand-in is a superset drawn for
    // ground not yet held, and exact-only colour waits for the band (§5.10).
    const standIn = this.standInBuffers(r.marks, column.colourBy);
    const colours = this.standInColours(standIn, useLut ? {kind: 'unmapped'} : encoding, useLut ? 'unmapped' : encodingKey);
    if (colours.length !== standIn.count * 4) {
      throw new Error(
        `colour buffer covers ${colours.length / 4} of ${standIn.count} stand-in marks. Colour is presentation and must never decide what is drawn.`
      );
    }
    layers.push(
      new ScatterplotLayer(
        this.getSubLayerProps({id: 'marks-standin'}),
        {
          visible: standIn.count > 0,
          data: {
            length: standIn.count,
            attributes: {getPosition: binary(standIn.positions, 2), getFillColor: binary(colours, 4, true)}
          },
          tesseraIds: standIn.ids,
          radiusUnits: 'pixels' as const,
          getRadius: this.props.radius,
          radiusMinPixels: 1,
          pickable: this.props.pickable,
          parameters: {depthCompare: 'always' as const}
        } as never
      )
    );

    this.props.onDrawn?.(slab.drawn, standIn.count);
    // The outlines go under everything: hairlines the wash and the marks show through.
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
  private warmMarksLayers(): Layer[] {
    return [
      new MarksLayer(
        this.getSubLayerProps({id: 'marks-p0'}),
        {
          visible: false,
          data: {length: 0, attributes: {getPosition: binary(EMPTY_F32, 2), getFillColor: binary(EMPTY_U8, 4, true), getOrdinal: binary(EMPTY_F32, 1)}},
          tesseraIds: EMPTY_IDS,
          useLut: false,
          lutTexture: null,
          radiusUnits: 'pixels' as const,
          getRadius: this.props.radius,
          pickable: false,
          parameters: {depthCompare: 'always' as const}
        } as never
      ),
      new ScatterplotLayer(
        this.getSubLayerProps({id: 'marks-standin'}),
        {
          visible: false,
          data: {length: 0, attributes: {getPosition: binary(EMPTY_F32, 2), getFillColor: binary(EMPTY_U8, 4, true)}},
          tesseraIds: EMPTY_IDS,
          radiusUnits: 'pixels' as const,
          getRadius: this.props.radius,
          pickable: false,
          parameters: {depthCompare: 'always' as const}
        } as never
      )
    ];
  }

  private standInBuffers(marks: MarksProjection, colourBy: string | null): StandInBuffers {
    const key = colourBy ?? '';
    const held = heldStandIn.get(marks.standIn);
    if (held && held.key === key) return held.buffers;
    const buffers = materialiseStandIn(marks.standIn, colourBy ? [colourBy] : []);
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
   * The served `hull` or `box` as hairline outlines, faded, in each artifact's own colour; the
   * opened artifact strong, with a faint fill that is the only coloured area fill on the map.
   * Derived per principal (contracts §3.2), so a shape is exact for this viewer; nothing is
   * contoured from held marks (decision 0099).
   */
  private outlineLayers(r: Resolved, timings: {outlinesMs: number; outlines: number}): Layer[] {
    const a = r.artifacts;
    const started = performance.now();
    const opened = this.props.openedArtifact ?? null;
    const scheme = this.props.scheme ?? 'dark';
    const key = a ? `${a.version}|${a.palette}|${opened ?? ''}|${scheme}` : '';
    let held = a ? heldOutlines.get(a.served) : undefined;
    if (a && (!held || held.key !== key)) {
      const data: OutlineDatum[] = [];
      for (const artifact of a.served) {
        const polygon = outlineOf(artifact);
        if (!polygon) continue;
        const ordinal = a.table.ordinalOf(artifact.layer, artifact.tesseraId);
        data.push({id: artifact.tesseraId, polygon, colour: a.colours.get(ordinal) ?? NEUTRAL, opened: artifact.tesseraId === opened});
      }
      held = {key, data};
      heldOutlines.set(a.served, held);
    }
    const data = held?.data ?? NO_OUTLINES;
    timings.outlinesMs = performance.now() - started;
    timings.outlines = data.length;
    // The layer exists from the first paint, empty, so its program is linked before it is needed.
    return [
      new PolygonLayer(
        this.getSubLayerProps({id: 'outlines'}),
        {
          visible: data.length > 0,
          data,
          getPolygon: (d: OutlineDatum) => d.polygon,
          filled: true,
          getFillColor: (d: OutlineDatum) => (d.opened ? [d.colour[0], d.colour[1], d.colour[2], 18] : [0, 0, 0, 0]),
          stroked: true,
          getLineColor: (d: OutlineDatum) => (d.opened ? [d.colour[0], d.colour[1], d.colour[2], 200] : [d.colour[0], d.colour[1], d.colour[2], HAIRLINE_ALPHA[scheme]]),
          lineWidthUnits: 'pixels' as const,
          getLineWidth: (d: OutlineDatum) => (d.opened ? 1.2 : 0.8),
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
   * Names and counts at each artifact's `centroid`, sized by masked count within a narrow band,
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
    const key = a ? `${a.version}|${a.palette}|${bucket}` : '';
    let held = a ? heldLabels.get(a.served) : undefined;
    if (a && viewport && (!held || held.key !== key)) {
      const placed = a.served.filter((x) => x.centroid !== null);
      // A dependent layer's artifacts — a clustering's topic labels — draw their text beneath the
      // name of whatever they sit on, italic and small, and are placed with it: they are not
      // candidates of their own (§5.10, D13).
      const dependent = new Set(r.meta?.layers.filter((l) => l.depsOn.length > 0).map((l) => l.name) ?? []);
      const topics = placed.filter((x) => dependent.has(x.layer));
      const named = placed.filter((x) => !dependent.has(x.layer));
      const largest = named.reduce((m, x) => Math.max(m, Number(x.maskedCount)), 1);
      const scale = 2 ** zoom; // pixels per world unit
      const candidates: LabelCandidate[] = [];
      const byId = new Map<bigint, {artifact: Artifact; name: string; countText: string; size: number; topic: string | null}>();
      for (const artifact of named) {
        const count = Number(artifact.maskedCount);
        const size = labelSize(count, largest);
        const name = artifactName(artifact);
        const countText = count.toLocaleString('en-GB');
        // The topic beneath: the dependent artifact nearest this centroid, within a label's reach.
        let topic: string | null = null;
        let best = Infinity;
        for (const t of topics) {
          const d = Math.hypot(t.centroid![0] - artifact.centroid![0], t.centroid![1] - artifact.centroid![1]) * (scale / GRID32_PER_WORLD);
          if (d < best && d < size * 6) {
            best = d;
            topic = t.content[0] ?? null;
          }
        }
        byId.set(artifact.tesseraId, {artifact, name, countText, size, topic});
        const width = (name.length * 0.56 + countText.length * 0.5 + 2) * size + 8;
        candidates.push({
          id: artifact.tesseraId,
          x: gridToWorld(artifact.centroid![0]) * scale,
          y: gridToWorld(artifact.centroid![1]) * scale,
          width: Math.max(width, topic ? topic.length * 11 * 0.5 : 0),
          height: size * 1.35 + (topic ? 13 : 0),
          priority: count
        });
      }
      const data: LabelDatum[] = [];
      const leaders: LeaderDatum[] = [];
      for (const p of placeLabels(candidates) as PlacedLabel[]) {
        const {artifact, name, countText, size, topic} = byId.get(p.id)!;
        const position = gridToWorldXY(artifact.centroid!);
        const ordinal = a.table.ordinalOf(artifact.layer, artifact.tesseraId);
        const colour = a.colours.get(ordinal) ?? NEUTRAL;
        // The name, then the count beside it — smaller and lighter — then the topic beneath.
        // The name ends and the count starts at one seam, so the width estimates cannot overlap.
        const nameWidth = name.length * 0.58 * size;
        const countWidth = countText.length * 0.55 * size * 0.82;
        const seam = p.dx - (nameWidth + countWidth + size * 0.35) / 2 + nameWidth;
        data.push({id: artifact.tesseraId, position, text: name, size, offset: [seam, p.dy], colour, kind: 'name'});
        data.push({id: artifact.tesseraId, position, text: countText, size: size * 0.82, offset: [seam + size * 0.35, p.dy + size * 0.08], colour, kind: 'count'});
        if (topic) data.push({id: artifact.tesseraId, position, text: topic, size: 11, offset: [p.dx, p.dy + size * 0.78 + 3], colour, kind: 'topic'});
        // A leader only where the label sits well clear of its centroid — a small nudge needs none.
        if (p.leader && Math.hypot(p.dx, p.dy) > size * 2.5) leaders.push({from: position, to: [position[0] + p.dx / scale, position[1] + p.dy / scale]});
      }
      held = {key, data, leaders};
      heldLabels.set(a.served, held);
    }
    const data = held?.data ?? NO_LABELS;
    const leaders = held?.leaders ?? NO_LEADERS;
    timings.labelsMs = performance.now() - started;
    timings.labels = data.length;
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
          getTextAnchor: (kind === 'name' ? 'end' : kind === 'count' ? 'start' : 'middle') as 'start' | 'middle' | 'end',
          getAlignmentBaseline: 'center' as const,
          fontFamily: 'IBM Plex Sans, system-ui, -apple-system, Segoe UI, Roboto, sans-serif',
          fontSettings: {sdf: true, buffer: 4},
          outlineWidth: kind === 'name' ? 2.5 : 2,
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
