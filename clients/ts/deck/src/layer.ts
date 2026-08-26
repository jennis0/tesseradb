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

const AMBER: [number, number, number, number] = [255, 210, 90, 255];
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
/** The wash image, once per `tiles` object. */
const heldWash = new WeakMap<object, {depth: number; image: ImageData | null; bounds: [number, number, number, number]}>();
/** The column encoding the slab's colour attribute holds, kept while the map colours by cluster. */
const heldColumnEncoding = new WeakMap<MarkSlab, {encoding: Encoding; colourBy: string | null}>();
/** A lookup texture per slab, for a host that handed none in. */
const ownLut = new WeakMap<MarkSlab, LookupTexture>();
/** The outline polygons, once per served set and opened artifact. */
const heldOutlines = new WeakMap<object, {key: string; data: OutlineDatum[]}>();
/** The label placement, once per served set and zoom bucket. */
const heldLabels = new WeakMap<object, {key: string; data: LabelDatum[]; leaders: LeaderDatum[]}>();

type OutlineDatum = {id: bigint; polygon: [number, number][]; colour: Rgba; opened: boolean};
type LabelDatum = {id: bigint; position: [number, number]; text: string; size: number; offset: [number, number]; colour: Rgba};
type LeaderDatum = {from: [number, number]; to: [number, number]};

const HAIRLINE_ALPHA = 110;

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
      return [...this.outlineLayers(r, timings), ...this.labelLayers(r, timings), ...this.selectionLayers()];
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

    if (this.props.wash && r.tiles) {
      const washStarted = performance.now();
      layers.push(this.washLayer(r.tiles, r.depth));
      timings.washMs = performance.now() - washStarted;
    }

    // Layers toggle `visible`; they are never omitted — deck destroys an absent layer and re-uploads
    // everything it held when it returns. One layer per retained slab partition, addressed by slot,
    // so a depth flip is a swap and flipping back uploads nothing.
    for (const held of slab.layers()) {
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

  /** The density wash, rebuilt once per `tiles` object — the settle's work, never the frame's. */
  private washLayer(tiles: TilesProjection, depth: number): Layer | null {
    let held = heldWash.get(tiles);
    if (!held || held.depth !== depth) {
      const binned = binDensity(tiles.tiles, depth);
      const image = binned && binned.filled > 0 ? filterDensity(binned, depth) : null;
      held = {
        depth,
        image: image && typeof ImageData !== 'undefined' ? new ImageData(image.data, image.width, image.height) : null,
        bounds: image ? image.bounds : [0, 0, 0, 0]
      };
      heldWash.set(tiles, held);
    }
    // No image is no layer: a BitmapLayer given no image throws in its texture transform, and a
    // wash that comes and goes is a few-kilobyte texture, not a re-upload worth keeping a layer for.
    if (!held.image) return null;
    const [x0, y0, x1, y1] = held.bounds;
    return new BitmapLayer(
      this.getSubLayerProps({id: 'wash'}),
      {
        image: held.image,
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
    if (!a || a.served.length === 0) return [];
    const started = performance.now();
    const opened = this.props.openedArtifact ?? null;
    const key = `${a.version}|${a.palette}|${opened ?? ''}`;
    let held = heldOutlines.get(a.served);
    if (!held || held.key !== key) {
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
    timings.outlinesMs = performance.now() - started;
    timings.outlines = held.data.length;
    if (held.data.length === 0) return [];
    return [
      new PolygonLayer(
        this.getSubLayerProps({id: 'outlines'}),
        {
          data: held.data,
          getPolygon: (d: OutlineDatum) => d.polygon,
          filled: true,
          getFillColor: (d: OutlineDatum) => (d.opened ? [d.colour[0], d.colour[1], d.colour[2], 36] : [0, 0, 0, 0]),
          stroked: true,
          getLineColor: (d: OutlineDatum) => (d.opened ? [d.colour[0], d.colour[1], d.colour[2], 255] : [d.colour[0], d.colour[1], d.colour[2], HAIRLINE_ALPHA]),
          lineWidthUnits: 'pixels' as const,
          getLineWidth: (d: OutlineDatum) => (d.opened ? 2 : 1),
          lineWidthMinPixels: 1,
          pickable: this.props.pickable,
          artifactIds: held.data.map((d) => d.id),
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
    const a = r.artifacts;
    if (!this.props.labels || !a || a.served.length === 0) return [];
    const viewport = this.context.viewport;
    if (!viewport) return [];
    const started = performance.now();
    const zoom = viewport.zoom;
    const bucket = Math.round(zoom * LABEL_ZOOM_STEP);
    const key = `${a.version}|${a.palette}|${bucket}`;
    let held = heldLabels.get(a.served);
    if (!held || held.key !== key) {
      const placed = a.served.filter((x) => x.centroid !== null);
      const largest = placed.reduce((m, x) => Math.max(m, Number(x.maskedCount)), 1);
      const scale = 2 ** zoom; // pixels per world unit
      const candidates: LabelCandidate[] = [];
      const byId = new Map<bigint, {artifact: Artifact; text: string; size: number}>();
      for (const artifact of placed) {
        const count = Number(artifact.maskedCount);
        const size = labelSize(count, largest);
        const name = artifactName(artifact);
        const countText = count.toLocaleString('en-GB');
        byId.set(artifact.tesseraId, {artifact, text: `${name}\n${countText}`, size});
        candidates.push({
          id: artifact.tesseraId,
          x: gridToWorld(artifact.centroid![0]) * scale,
          y: gridToWorld(artifact.centroid![1]) * scale,
          width: Math.max(name.length, countText.length) * size * 0.62 + 8,
          height: size * 2.4 + 4,
          priority: count
        });
      }
      const data: LabelDatum[] = [];
      const leaders: LeaderDatum[] = [];
      for (const p of placeLabels(candidates) as PlacedLabel[]) {
        const {artifact, text, size} = byId.get(p.id)!;
        const position = gridToWorldXY(artifact.centroid!);
        const ordinal = a.table.ordinalOf(artifact.layer, artifact.tesseraId);
        data.push({id: artifact.tesseraId, position, text, size, offset: [p.dx, p.dy], colour: a.colours.get(ordinal) ?? NEUTRAL});
        if (p.leader) leaders.push({from: position, to: [position[0] + p.dx / scale, position[1] + p.dy / scale]});
      }
      held = {key, data, leaders};
      heldLabels.set(a.served, held);
    }
    timings.labelsMs = performance.now() - started;
    timings.labels = held.data.length;
    const layers: Layer[] = [];
    if (held.leaders.length > 0) {
      layers.push(
        new LineLayer(
          this.getSubLayerProps({id: 'label-leaders'}),
          {
            data: held.leaders,
            getSourcePosition: (d: LeaderDatum) => d.from,
            getTargetPosition: (d: LeaderDatum) => d.to,
            getColor: [200, 205, 212, 140],
            widthUnits: 'pixels' as const,
            getWidth: 1,
            pickable: false,
            parameters: {depthCompare: 'always' as const}
          } as never
        )
      );
    }
    layers.push(
      new TextLayer(
        this.getSubLayerProps({id: 'labels'}),
        {
          data: held.data,
          getPosition: (d: LabelDatum) => d.position,
          getText: (d: LabelDatum) => d.text,
          getSize: (d: LabelDatum) => d.size,
          sizeUnits: 'pixels' as const,
          getColor: [240, 243, 247, 255],
          getPixelOffset: (d: LabelDatum) => d.offset,
          getTextAnchor: 'middle' as const,
          getAlignmentBaseline: 'center' as const,
          fontFamily: 'Inter, system-ui, -apple-system, Segoe UI, Roboto, sans-serif',
          fontWeight: 600,
          fontSettings: {sdf: true, buffer: 4},
          outlineWidth: 5,
          outlineColor: [8, 10, 14, 220],
          characterSet: 'auto',
          lineHeight: 1.1,
          pickable: this.props.pickable,
          artifactIds: held.data.map((d) => d.id),
          parameters: {depthCompare: 'always' as const},
          updateTriggers: {getPixelOffset: key, getSize: key}
        } as never
      )
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
    if (shape) {
      layers.push(
        new PolygonLayer(
          this.getSubLayerProps({id: 'region'}),
          {
            data: [{polygon: shape}],
            getPolygon: (d: {polygon: number[][]}) => d.polygon,
            filled: shape.length >= 3,
            getFillColor: [255, 210, 90, 28],
            stroked: true,
            getLineColor: AMBER,
            lineWidthUnits: 'pixels' as const,
            getLineWidth: live ? 1 : 1.5,
            pickable: false,
            parameters: {depthCompare: 'always' as const}
          } as never
        )
      );
    }
    const at = this.props.selectedWorldXY;
    if (at) {
      layers.push(
        new ScatterplotLayer(
          this.getSubLayerProps({id: 'picked'}),
          {
            data: [at],
            getPosition: (d: [number, number]) => d,
            getFillColor: AMBER,
            radiusUnits: 'pixels' as const,
            getRadius: 5,
            stroked: true,
            getLineColor: [20, 20, 20, 255],
            lineWidthUnits: 'pixels' as const,
            getLineWidth: 1.5,
            pickable: false,
            parameters: {depthCompare: 'always' as const}
          } as never
        )
      );
    }
    return layers;
  }
}
