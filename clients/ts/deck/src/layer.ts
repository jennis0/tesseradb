import {CompositeLayer, type BinaryAttribute as DeckBinaryAttribute, type CompositeLayerProps, type Layer, type LayersList, type UpdateParameters} from '@deck.gl/core';
import {BitmapLayer, PolygonLayer, ScatterplotLayer, TextLayer} from '@deck.gl/layers';
import {
  WORLD_SIZE,
  type ArtifactsProjection,
  type LegendProjection,
  type MarksProjection,
  type Meta,
  type PresentedStatus,
  type Store,
  type TilesProjection
} from '@tesseradb/client';
import {materialiseStandIn, type StandInBuffers} from './assemble.js';
import {buildColourAttribute, type Encoding} from './colour.js';
import {binDensity} from './density.js';
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
 * The drawing, in order (§5.10): the density wash from the exact tiles' counts, the marks — one
 * `ScatterplotLayer` per retained slab partition, addressed by slot, plus the stand-ins — the
 * served artifacts as a marker and count at their wire `centroid`, the selected mark, and the
 * selected region as the shape drawn (decision 0097: never a grid). ⊘ Hull and box outlines and
 * the membership lookup texture are step 3's; until then artifacts are markers and counts.
 */

/** Wire artifact geometry is 32 bits per axis (contracts §3.2); the world is 512 units across. */
export const GRID32_PER_WORLD_UNIT = 2 ** 32 / WORLD_SIZE;

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
  /** The picked mark's world position, for its marker. */
  selectedWorldXY?: [number, number] | null;
  openedArtifact?: bigint | null;
  /** The selected region's world bbox, and the live shape while a box is being dragged. */
  region?: [number, number, number, number] | null;
  drag?: [number, number, number, number] | null;
  /** Whether the density wash is drawn under the points. */
  wash?: boolean;
  radius?: number;
  /** How many marks a paint ended up drawing, for the host's probe. */
  onDrawn?: ((drawn: number, provisional: number) => void) | null;
};

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
  if (!colourBy || !meta || !legend) return {kind: 'uniform'};
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
      instancePickingColors: {buffer: gpu.picking, size: 4, type: 'uint8', stride: 4, offset: 0}
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
    selectedWorldXY: null,
    openedArtifact: null,
    region: null,
    drag: null,
    wash: true,
    radius: 1.6,
    pickable: true,
    onDrawn: null
  };

  declare state: {tick: number; unsubscribe: (() => void) | null; subscribed: Store | null};

  override initializeState(): void {
    this.state = {tick: 0, unsubscribe: null, subscribed: null};
    this.follow(this.props.store ?? null);
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

  override renderLayers(): LayersList {
    const r = this.resolved();
    const {slab} = this.props;
    const layers: (Layer | null)[] = [];

    // A refusal draws no marks but keeps the slab: the held bands are still the answer to the last
    // view that succeeded, and discarding them would make recovery pay for a full rewrite. So a
    // refused view is blank — the map element paints the state over it — and never an empty corpus.
    if (r.status === 'refused' || !r.marks || r.marks.bands.length === 0 && r.marks.standIn.length === 0 && !r.marks.count.exact) {
      if (!r.marks) slab.clear();
      this.props.onDrawn?.(0, 0);
      return [...this.artifactLayers(r), ...this.selectionLayers()];
    }

    const encoding = encodingOf(r.meta, r.legend);
    const encodingKey = encodingSignature(encoding);
    const colourBy = r.legend?.colourBy ?? null;
    slab.sync(r.marks.bands, r.depth, encoding, colourBy);

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

    if (this.props.wash && r.tiles) layers.push(this.washLayer(r.tiles, r.depth));

    // Layers toggle `visible`; they are never omitted — deck destroys an absent layer and re-uploads
    // everything it held when it returns. One layer per retained slab partition, addressed by slot,
    // so a depth flip is a swap and flipping back uploads nothing.
    for (const held of slab.layers()) {
      layers.push(
        new ScatterplotLayer(
          this.getSubLayerProps({id: `marks-p${held.slot}`}),
          {
            visible: held.active && held.draw.length > 0,
            data: {
              length: held.draw.length,
              attributes: held.draw.gpu
                ? gpuAttributes(held.draw.gpu)
                : {getPosition: binary(held.draw.positions, 2), getFillColor: binary(held.draw.colours, 4, true)}
            },
            tesseraIds: held.draw.ids,
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
    // number channel — no count is shown against a non-exact tile — not the alpha channel.
    const standIn = this.standInBuffers(r.marks, colourBy);
    const colours = this.standInColours(standIn, encoding, encodingKey);
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
    layers.push(...this.artifactLayers(r), ...this.selectionLayers());
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
      const image = binDensity(tiles.tiles, depth);
      held = {
        depth,
        image: image && image.filled > 0 && typeof ImageData !== 'undefined' ? new ImageData(image.data, image.width, image.height) : null,
        bounds: image ? image.bounds : [0, 0, 0, 0]
      };
      heldWash.set(tiles, held);
    }
    const [x0, y0, x1, y1] = held.bounds;
    return new BitmapLayer(
      this.getSubLayerProps({id: 'wash'}),
      {
        visible: held.image !== null,
        image: held.image ?? undefined,
        // `[left, bottom, right, top]`: row 0 of the image is the lowest tile row, and under the
        // y-down orthographic view that is the smaller world y — so `top` is `y0`.
        bounds: [x0, y1, x1, y0],
        pickable: false,
        // Nearest-neighbour: a bin is a tile, and interpolating across bins would draw density
        // where the counts said none.
        textureParameters: {minFilter: 'nearest', magFilter: 'nearest'},
        parameters: {depthCompare: 'always' as const}
      } as never
    );
  }

  /**
   * The served artifacts as a marker and a count at their wire `centroid` — derived per principal,
   * so where a marker sits is a fact about this viewer's own members. The radius encodes the masked
   * count by square root, against the largest count in this view for this principal: a comparison
   * between this principal's counts, never a fraction of a size the viewer was not given.
   */
  private artifactLayers(r: Resolved): Layer[] {
    const served = r.artifacts?.served ?? [];
    const placed = served.filter((a) => a.centroid !== null);
    if (placed.length === 0) return [];
    const opened = this.props.openedArtifact ?? null;
    const data = placed.map((a) => ({
      id: a.tesseraId,
      count: Number(a.maskedCount),
      opened: a.tesseraId === opened,
      position: [a.centroid![0] / GRID32_PER_WORLD_UNIT, a.centroid![1] / GRID32_PER_WORLD_UNIT] as [number, number]
    }));
    const largest = data.reduce((m, d) => Math.max(m, d.count), 1);
    const radiusOf = (count: number) => 3 + 5 * Math.sqrt(count / largest);
    const openedKey = String(opened ?? '');
    return [
      new ScatterplotLayer(
        this.getSubLayerProps({id: 'artifact-markers'}),
        {
          data,
          getPosition: (d: (typeof data)[number]) => d.position,
          getRadius: (d: (typeof data)[number]) => radiusOf(d.count),
          radiusUnits: 'pixels' as const,
          filled: true,
          getFillColor: CHROME,
          stroked: true,
          getLineColor: (d: (typeof data)[number]) => (d.opened ? AMBER : PLATE),
          lineWidthUnits: 'pixels' as const,
          getLineWidth: (d: (typeof data)[number]) => (d.opened ? 2.5 : 1.5),
          pickable: this.props.pickable,
          radiusMinPixels: 4,
          artifactIds: data.map((d) => d.id),
          parameters: {depthCompare: 'always' as const},
          updateTriggers: {getRadius: largest, getLineColor: openedKey, getLineWidth: openedKey}
        } as never
      ),
      new TextLayer(
        this.getSubLayerProps({id: 'artifact-counts'}),
        {
          data,
          getPosition: (d: (typeof data)[number]) => d.position,
          getText: (d: (typeof data)[number]) => d.count.toLocaleString('en-GB'),
          getSize: 11,
          sizeUnits: 'pixels' as const,
          getColor: [234, 238, 243, 255],
          fontFamily: 'SFMono-Regular, Menlo, monospace',
          getPixelOffset: (d: (typeof data)[number]) => [0, -(radiusOf(d.count) + 9)],
          background: true,
          getBackgroundColor: PLATE,
          backgroundPadding: [4, 2, 4, 2],
          getBorderColor: [34, 34, 34, 255],
          getBorderWidth: 1,
          fontSettings: {sdf: true},
          fontWeight: 600,
          characterSet: 'auto',
          pickable: false,
          parameters: {depthCompare: 'always' as const},
          updateTriggers: {getPixelOffset: largest}
        } as never
      )
    ];
  }

  /** The picked mark's marker, and the selected region as the shape drawn — never its cells. */
  private selectionLayers(): Layer[] {
    const layers: Layer[] = [];
    const shape = this.props.drag ?? this.props.region ?? null;
    if (shape) {
      const [x0, y0, x1, y1] = shape;
      layers.push(
        new PolygonLayer(
          this.getSubLayerProps({id: 'region'}),
          {
            data: [{polygon: [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]}],
            getPolygon: (d: {polygon: number[][]}) => d.polygon,
            filled: true,
            getFillColor: [255, 210, 90, 28],
            stroked: true,
            getLineColor: AMBER,
            lineWidthUnits: 'pixels' as const,
            getLineWidth: this.props.drag ? 1 : 1.5,
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
