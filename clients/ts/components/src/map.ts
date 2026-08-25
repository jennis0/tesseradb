import {Deck, OrthographicView, type Layer, type PickingInfo} from '@deck.gl/core';
import {css, html, nothing, type PropertyValues} from 'lit';
import {property, state} from 'lit/decorators.js';
import {
  MAX_DEPTH,
  WORLD_SIZE,
  assertCompositionMatchesServed,
  dataToWorldXY,
  worldBbox,
  type Store,
  type SelectionShape
} from '@tesseradb/client';
import {LookupTexture, MarkSlab, TesseraLayer, clusterLayerOf, encodingOf, encodingSignature, resolvePick, type Picked} from '@tesseradb/deck';
import type {PaletteKind} from '@tesseradb/client';
import {TesseraElement, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import type {PickOutcome} from './item-card.js';
import {renderState, stateOf, type PanelState} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-map>` — the canvas (design client-components §5.3 tier 1, §5.9): points, the density
 * wash, the artifact markers, hover, pick, the selection highlight, and the display states drawn
 * over the canvas itself, so a refused or expired view never reads as an empty corpus.
 *
 * **Per instance, not per module.** Each map owns its `Deck`, its `MarkSlab` and its probe. The
 * `Deck` is finalised **a settle after** disconnection, cancelled if the element reconnects first
 * — JupyterLab's windowed notebooks and a framework's reorder disconnect and reconnect elements
 * routinely, and a synchronous finalize would rebuild the GPU slab on every scroll-past. The
 * store is never disposed on disconnect (`TesseraElement`).
 *
 * **The map owns the camera; the store is told** (§4). deck's `{target, zoom}` in world space is
 * converted to the view's data bbox and handed to `setView` on every change; the driver debounces.
 * The element holds the view state itself so `fit()`, `fitTo()` and the keyboard can move it.
 *
 * **Selection** (§5.11): `mode="box"`, or shift-drag in `pan`, draws a box; `mode="lasso"` draws
 * a freehand polygon. The highlight while dragging is the shape and nothing else (decision 0097),
 * and the settled shape goes to `store.select`, which counts it over the cells it meets.
 *
 * **Colour by cluster** is `colour-by="cluster:<layer>"` (§5.10): the map owns the lookup texture
 * beside the slab, and the `palette` property chooses positional or spread (decision 0099).
 *
 * `display: block` with its height from `--tessera-map-height`, because a custom element is
 * inline and heightless and deck sizes its canvas from its parent.
 */

/** How long after disconnection the `Deck` is finalised, unless the element reconnects. */
const FINALIZE_SETTLE_MS = 250;

/** What the map publishes for an instrument or a smoke script — mutated in place, one object. */
export type MapProbe = {
  paints: number;
  at: number;
  marks: number;
  /** Viewport requests the store has issued, when its instruments report them; else 0. */
  requests: number;
  encoding: string;
  view: {depth: number; status: string; stale: boolean; visible: number; matched: number; served: number; provisional: number};
  /** `ms` is select-to-counted, the store's own clock: the settle, the request and the sum. */
  region: {depth: number; tiles: number; exact: boolean; visible: number; matched: number; held: number; status: string; ms: number | null} | null;
  timings: {
    /** Per settle: the slab sync, the wash bin, the lookup texture, the outlines, the labels and the whole layer build, last values in ms. */
    slabMs: number;
    washMs: number;
    lutMs: number;
    outlinesMs: number;
    labelsMs: number;
    layersMs: number;
    /** Lookup-texture writes since the map was made — what a colouring interaction costs. */
    lutWrites: number;
    /** Frame gaps over the last two seconds, ms. */
    frame: {mean: number; p95: number; n: number};
    /** Per-response decode, reported by the host through the store's instruments. */
    decodeMs: number[];
  };
  /**
   * Colour by cluster, for the harness: the layer coloured by, the coverage, and a sample of
   * the ordinals the marks on screen carry with what each resolves to — a served artifact's id,
   * or none — so *a coloured point's ordinal resolves to a served artifact* is checked rather
   * than eyeballed.
   */
  cluster: {
    layer: string | null;
    layersOn: string[];
    coverage: {current: number; stale: number};
    servedIds: string[];
    sample: {ordinal: number; resolvedId: string | null}[];
  };
  [extra: string]: unknown;
};

type ViewState = {target: [number, number, number]; zoom: number; minZoom: number; maxZoom: number};

const VIEW = new OrthographicView({id: 'ortho', flipY: true});

export class TesseraMap extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
        position: relative;
        height: var(--tessera-map-height);
        min-height: 120px;
        background: var(--tessera-map-bg);
        outline: none;
        overflow: hidden;
      }
      :host(:focus-visible) {
        box-shadow: inset 0 0 0 2px var(--tessera-accent);
      }
      [part='canvas'] {
        position: absolute;
        inset: 0;
      }
      :host([mode='box']) [part='canvas'],
      :host([mode='lasso']) [part='canvas'] {
        cursor: crosshair;
      }
      .corner {
        position: absolute;
        z-index: 2;
        display: flex;
        flex-direction: column;
        gap: var(--tessera-space);
        max-width: 46%;
        pointer-events: none;
      }
      .corner > ::slotted(*) {
        pointer-events: auto;
      }
      .top-left {
        top: var(--tessera-space);
        left: var(--tessera-space);
      }
      .top-right {
        top: var(--tessera-space);
        right: var(--tessera-space);
      }
      .bottom-left {
        bottom: var(--tessera-space);
        left: var(--tessera-space);
      }
      .bottom-right {
        bottom: var(--tessera-space);
        right: var(--tessera-space);
        align-items: flex-end;
      }
      [part='controls'] {
        display: flex;
        gap: 4px;
        pointer-events: auto;
      }
      [part='controls'] button {
        padding: 2px 8px;
      }
      [part='controls'] button[aria-pressed='true'] {
        border-color: var(--tessera-accent);
        color: var(--tessera-accent);
      }
      [part='tooltip'] {
        position: absolute;
        z-index: 4;
        pointer-events: none;
        padding: 4px 8px;
        background: var(--tessera-panel-bg);
        border: 1px solid var(--tessera-border);
        border-radius: var(--tessera-radius);
        font-size: var(--tessera-font-size-small);
        white-space: nowrap;
        transform: translate(12px, 12px);
      }
      [part='overlay'] {
        position: absolute;
        z-index: 2;
        inset: 0;
        display: flex;
        align-items: center;
        justify-content: center;
        pointer-events: none;
      }
      [part='overlay'] [part='state'] {
        pointer-events: auto;
        padding: var(--tessera-space) calc(var(--tessera-space) * 2);
        background: var(--tessera-panel-bg);
        border: 1px solid var(--tessera-border);
        border-radius: var(--tessera-radius);
      }
    `
  ];

  protected override canBuildOwn = true;

  @property({attribute: 'colour-by'}) accessor colourBy = '';
  @property({
    attribute: 'layers',
    converter: {fromAttribute: (v: string | null) => (v ? v.split(/[\s,]+/).filter(Boolean) : []), toAttribute: (v: string[]) => v.join(' ')}
  })
  accessor layers: string[] | null = null;
  @property({type: Number}) accessor budget = 0;
  @property({attribute: 'tooltip-fields'}) accessor tooltipFields = '';
  @property({reflect: true}) accessor mode: 'pan' | 'box' | 'lasso' = 'pan';
  /** How served artifacts are coloured: by position about the extent's centre, or spread over the served set. */
  @property() accessor palette: PaletteKind = 'positional';
  /** The level to colour a nested layer at; unset colours at the deepest served. */
  @property({type: Number, attribute: 'cluster-level'}) accessor clusterLevel: number | null = null;
  /** A deck.gl layer drawn under the points — a geographic corpus's basemap (§5.3). */
  @property({attribute: false}) accessor basemap: Layer | null = null;
  @property({type: Boolean}) accessor wash = true;
  @property({type: Number}) accessor radius = 1.6;
  /** The mode and fit control cluster — the map's own, not a slot. */
  @property({type: Boolean, attribute: 'no-controls'}) accessor noControls = false;

  @state() accessor hover: {x: number; y: number; lines: string[]} | null = null;
  @state() accessor drag: [number, number, number, number] | null = null;
  @state() accessor dragPolygon: [number, number][] | null = null;

  /** What the last click resolved to — a miss, a broken pick, or a hit that went to the store. */
  lastPick: PickOutcome = null;
  /** The map's probe, one object mutated in place; the demo publishes the first map's on `window`. */
  readonly probe: MapProbe = {
    paints: 0,
    at: 0,
    marks: 0,
    requests: 0,
    encoding: 'uniform',
    view: {depth: 0, status: 'idle', stale: false, visible: 0, matched: 0, served: 0, provisional: 0},
    region: null,
    timings: {slabMs: 0, washMs: 0, lutMs: 0, outlinesMs: 0, labelsMs: 0, layersMs: 0, lutWrites: 0, frame: {mean: 0, p95: 0, n: 0}, decodeMs: []},
    cluster: {layer: null, layersOn: [], coverage: {current: 0, stale: 0}, servedIds: [], sample: []}
  };

  readonly slab = new MarkSlab();
  readonly lut = new LookupTexture();
  private deck: Deck<OrthographicView> | null = null;
  private finalizeTimer: ReturnType<typeof setTimeout> | null = null;
  private viewState: ViewState = {target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0], zoom: 0, minZoom: -2, maxZoom: MAX_DEPTH};
  private selectedWorldXY: [number, number] | null = null;
  private regionWorld: [number, number, number, number] | null = null;
  private regionPolygon: [number, number][] | null = null;
  private regionAnnounced: object | null = null;
  private regionAskedAt = 0;
  private pickedId: bigint | null = null;
  private announcedItem: object | null = null;
  private announcedArtifact: object | null = null;
  private dragStart: [number, number] | null = null;
  private metaSeen = false;
  private frameGaps: number[] = [];
  private frameLoop: number | null = null;
  private lastFrameAt = 0;

  // ---- lifecycle ---------------------------------------------------------------------------

  override connectedCallback(): void {
    super.connectedCallback();
    if (this.finalizeTimer) {
      clearTimeout(this.finalizeTimer);
      this.finalizeTimer = null;
    }
    if (!this.hasAttribute('tabindex')) this.tabIndex = 0;
    this.setAttribute('role', 'application');
    if (!this.hasAttribute('aria-label')) this.setAttribute('aria-label', 'map — arrow keys pan, + and - zoom');
    this.addEventListener('keydown', this.onKey);
    this.startFrameLoop();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.removeEventListener('keydown', this.onKey);
    this.stopFrameLoop();
    this.finalizeTimer = setTimeout(() => {
      this.finalizeTimer = null;
      this.deck?.finalize();
      this.deck = null;
    }, FINALIZE_SETTLE_MS);
  }

  protected override updated(changed: PropertyValues<this>): void {
    this.ensureDeck();
    const s = this.resolvedStore;
    if (s) {
      if (changed.has('colourBy') && this.colourBy !== '') s.setColourBy(this.colourBy === 'none' ? null : this.colourBy);
      if (changed.has('layers') && this.layers) {
        s.setLayers(this.layers);
        emit(this, 'tessera-layerchange', {layers: this.layers});
      }
      if (changed.has('budget') && this.budget > 0) s.setBudget(this.budget);
      if (changed.has('palette')) s.setPalette(this.palette);
    }
    if (changed.has('mode') || changed.has('drag') || changed.has('dragPolygon') || changed.has('basemap') || changed.has('wash') || changed.has('radius') || changed.has('clusterLevel')) this.paint();
  }

  protected override onStoreAdopted(store: Store): void {
    this.slab.clear();
    this.metaSeen = false;
    this.selectedWorldXY = null;
    this.regionWorld = null;
    this.regionPolygon = null;
    if (this.colourBy !== '') store.setColourBy(this.colourBy === 'none' ? null : this.colourBy);
    if (this.layers) store.setLayers(this.layers);
    if (this.budget > 0) store.setBudget(this.budget);
    if (this.palette !== 'positional') store.setPalette(this.palette);
    this.paint();
  }

  protected override onStoreChange(): void {
    const s = this.resolvedStore;
    if (!s) return;
    if (!this.metaSeen && s.get('meta')) {
      this.metaSeen = true;
      this.pushView();
    }
    const view = s.get('view');
    const status = s.get('status');
    const p = this.probe;
    p.view = {
      depth: view.depth,
      status: status.status,
      stale: status.stale,
      visible: view.visible.value,
      matched: view.matched.value,
      served: view.served.shown,
      provisional: view.provisional
    };
    const legend = s.get('legend');
    const clusterLayer = clusterLayerOf(legend.colourBy);
    p.encoding = clusterLayer ? `cluster|${clusterLayer}` : encodingSignature(encodingOf(s.get('meta'), legend));
    if (view.composition && !checkedCompositions.has(view.composition)) {
      checkedCompositions.add(view.composition);
      assertCompositionMatchesServed(view.composition);
    }
    const artifacts = s.get('artifacts');
    if (artifacts !== this.probedArtifacts || view.composition !== this.probedComposition) {
      this.probedArtifacts = artifacts;
      this.probedComposition = view.composition;
      p.cluster = this.clusterProbe(clusterLayer, artifacts, s.get('marks').bands);
    }

    // Events for what arrived: the picked record, the opened artifact, the region's counts.
    const sel = s.get('selection');
    if (sel.item && sel.item !== this.announcedItem) {
      this.announcedItem = sel.item;
      emit(this, 'tessera-pick', {id: idString(sel.item.id), record: sel.item.detail});
    }
    if (sel.artifact && sel.artifact !== this.announcedArtifact) {
      this.announcedArtifact = sel.artifact;
      emit(this, 'tessera-artifactopen', {id: idString(sel.artifact.id), detail: sel.artifact.detail});
    }
    const region = s.get('region');
    const meta = s.get('meta');
    if (region && meta) {
      const q = meta.quantisation;
      if (region.shape.kind === 'box') {
        const [x0, y0] = dataToWorldXY(region.shape.bbox[0], region.shape.bbox[1], q);
        const [x1, y1] = dataToWorldXY(region.shape.bbox[2], region.shape.bbox[3], q);
        const next: [number, number, number, number] = [Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)];
        if (!this.regionWorld || next.some((v, i) => v !== this.regionWorld![i])) {
          this.regionWorld = next;
          this.regionPolygon = null;
          this.paint();
        }
      } else if (region.shape !== this.regionShape) {
        this.regionShape = region.shape;
        this.regionWorld = null;
        this.regionPolygon = region.shape.points.map(([x, y]) => dataToWorldXY(x, y, q));
        this.paint();
      }
      const ms = region.status === 'loading' ? null : (p.region?.ms ?? performance.now() - this.regionAskedAt);
      p.region = {depth: region.depth, tiles: region.tiles, exact: region.visible.exact, visible: region.visible.value, matched: region.matched.value, held: region.held.count, status: region.status, ms};
      if (region.status !== 'loading' && region !== this.regionAnnounced) {
        this.regionAnnounced = region;
        emit(this, 'tessera-selectchange', {
          shape: region.shape,
          status: region.status,
          visible: region.visible,
          matched: region.matched,
          served: region.served,
          depth: region.depth
        });
      }
    } else if (!region && (this.regionWorld || this.regionPolygon)) {
      this.regionWorld = null;
      this.regionPolygon = null;
      this.regionShape = null;
      p.region = null;
      this.paint();
    }
    super.onStoreChange();
  }

  private probedArtifacts: object | null = null;
  private probedComposition: object | null = null;
  private regionShape: SelectionShape | null = null;

  /** See {@link MapProbe.cluster}: a sample of carried ordinals, each resolved through the table. */
  private clusterProbe(clusterLayer: string | null, artifacts: ReturnType<Store['get']> & {layers: string[]}, bands: readonly {membership: Record<string, {distinct: Uint32Array}>}[]): MapProbe['cluster'] {
    const a = artifacts as unknown as import('@tesseradb/client').ArtifactsProjection;
    const layer = clusterLayer ?? a.layers[0] ?? null;
    const sample: {ordinal: number; resolvedId: string | null}[] = [];
    if (layer) {
      for (const band of bands) {
        const m = band.membership[layer];
        if (!m) continue;
        for (let i = 0; i < m.distinct.length && sample.length < 16; i++) {
          const ordinal = m.distinct[i]!;
          const resolved = a.table.resolve(ordinal, a.servedOrdinals, this.clusterLevel ?? undefined);
          const entry = resolved === 0 ? null : a.table.entry(resolved);
          sample.push({ordinal, resolvedId: entry ? idString(entry.tesseraId) : null});
        }
        if (sample.length >= 16) break;
      }
    }
    return {layer, layersOn: a.layers, coverage: a.coverage, servedIds: a.served.map((x) => idString(x.tesseraId)), sample};
  }

  /** Release the store this map built, and the `Deck`, now. */
  override dispose(): void {
    super.dispose();
    this.deck?.finalize();
    this.deck = null;
    this.slab.clear();
    this.lut.destroy();
  }

  // ---- deck ---------------------------------------------------------------------------------

  private ensureDeck(): void {
    if (this.deck || !this.isConnected) return;
    const parent = this.renderRoot.querySelector<HTMLDivElement>('[part="canvas"]');
    if (!parent) return;
    this.deck = new Deck<OrthographicView>({
      parent,
      views: VIEW,
      viewState: this.viewState,
      controller: true,
      pickingRadius: 8,
      layers: [],
      onDeviceInitialized: (device) => {
        this.slab.attach(device);
        this.lut.attach(device);
      },
      onViewStateChange: ({viewState}) => {
        const v = viewState as {target: number[]; zoom: number};
        this.viewState = {...this.viewState, target: [v.target[0]!, v.target[1]!, 0], zoom: v.zoom};
        this.deck?.setProps({viewState: this.viewState});
        this.pushView();
        return viewState;
      },
      onClick: (info) => this.onClick(info),
      onHover: (info) => this.onHover(info)
    });
    this.paint();
  }

  private get size(): {width: number; height: number} {
    return {width: this.clientWidth || 1, height: this.clientHeight || 1};
  }

  /** Tell the store where the camera is looking, in data coordinates. */
  private pushView(): void {
    const s = this.resolvedStore;
    if (!s || !s.get('meta')) return;
    const {width, height} = this.size;
    const v = this.viewState;
    const wb = worldBbox({target: [v.target[0], v.target[1]], zoom: v.zoom, width, height}, 1);
    const [x0, y0] = s.dataXY(wb[0], wb[1]);
    const [x1, y1] = s.dataXY(wb[2], wb[3]);
    s.setView({bbox: [x0, y0, x1, y1], width, height});
    emit(this, 'tessera-viewchange', {bbox: [x0, y0, x1, y1], zoom: v.zoom, width, height});
  }

  private paint(): void {
    if (!this.deck) return;
    const s = this.resolvedStore;
    const layers: Layer[] = [];
    if (this.basemap) layers.push(this.basemap);
    if (s) {
      layers.push(
        new TesseraLayer({
          id: 'tessera',
          store: s,
          slab: this.slab,
          lut: this.lut,
          clusterLevel: this.clusterLevel ?? undefined,
          selectedWorldXY: this.selectedWorldXY,
          openedArtifact: s.get('selection').artifact?.id ?? null,
          region: this.regionWorld,
          regionPolygon: this.regionPolygon,
          drag: this.drag,
          dragPolygon: this.dragPolygon,
          wash: this.wash,
          radius: this.radius,
          onDrawn: (drawn, provisional) => {
            const p = this.probe;
            p.paints += 1;
            p.at = performance.now();
            p.marks = drawn + provisional;
          },
          onTimings: (t) => {
            Object.assign(this.probe.timings, {
              slabMs: t.slabMs,
              washMs: t.washMs,
              lutMs: t.lutMs,
              outlinesMs: t.outlinesMs,
              labelsMs: t.labelsMs,
              layersMs: t.layersMs,
              lutWrites: t.lutWrites
            });
          }
        })
      );
    }
    this.deck.setProps({
      layers,
      viewState: this.viewState,
      // In box and lasso modes the drag is the selection's; the wheel still zooms.
      controller: this.mode === 'box' || this.mode === 'lasso' ? {dragPan: false, dragRotate: false} : true
    });
  }

  // ---- hover and pick -----------------------------------------------------------------------

  private onHover(info: PickingInfo): void {
    const picked = resolvePick(info as never);
    if (picked.kind !== 'mark') {
      if (this.hover) this.hover = null;
      return;
    }
    const lines = [`#${idString(picked.id)}`];
    const fields = this.tooltipFields.split(/[\s,]+/).filter(Boolean);
    const layerId = (info.sourceLayer ?? info.layer)?.id ?? '';
    const slot = /marks-p(\d+)$/.exec(layerId);
    if (fields.length > 0 && slot) {
      const at = this.slab.markAt(Number(slot[1]), info.index);
      if (at) {
        for (const f of fields) {
          const column = at.band.scalars[f];
          if (!column) continue;
          const raw = (column.values as ArrayLike<unknown>)[at.i];
          lines.push(`${f}: ${column.arrowType === 'timestamp_us' ? new Date(Number(raw) / 1000).toISOString().slice(0, 10) : String(raw)}`);
        }
      }
    }
    this.hover = {x: info.x, y: info.y, lines};
    emit(this, 'tessera-hover', {id: idString(picked.id), x: info.x, y: info.y});
  }

  private onClick(info: PickingInfo): void {
    const s = this.resolvedStore;
    const picked: Picked = resolvePick(info as never);
    switch (picked.kind) {
      case 'artifact':
        this.lastPick = null;
        void s?.openArtifact(picked.id);
        return;
      case 'mark':
        this.lastPick = null;
        this.pickedId = picked.id;
        this.selectedWorldXY = picked.worldXY;
        emit(this, 'tessera-pick', {id: idString(picked.id)});
        void s?.pick(picked.id);
        this.paint();
        return;
      case 'miss':
        this.lastPick = {kind: 'miss'};
        return;
      case 'broken':
        this.lastPick = picked;
        return;
    }
  }

  // ---- selection: the box and the lasso -------------------------------------------------------

  private unproject(e: PointerEvent): [number, number] | null {
    const viewport = this.deck?.getViewports()[0];
    if (!viewport) return null;
    const rect = this.getBoundingClientRect();
    const xy = viewport.unproject([e.clientX - rect.left, e.clientY - rect.top]);
    return [xy[0]!, xy[1]!];
  }

  private onPointerDown = (e: PointerEvent): void => {
    if (e.button !== 0) return;
    const lasso = this.mode === 'lasso';
    if (!lasso && this.mode !== 'box' && !(this.mode === 'pan' && e.shiftKey)) return;
    const at = this.unproject(e);
    if (!at) return;
    this.dragStart = at;
    if (lasso) this.dragPolygon = [at];
    else this.drag = [at[0], at[1], at[0], at[1]];
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    e.stopPropagation();
  };

  private onPointerMove = (e: PointerEvent): void => {
    if (!this.dragStart) return;
    const at = this.unproject(e);
    if (!at) return;
    if (this.dragPolygon) {
      // A vertex per pointer move, thinned to a pixel or so: the shape the user drew, no more.
      const last = this.dragPolygon[this.dragPolygon.length - 1]!;
      const scale = 2 ** this.viewState.zoom;
      if (Math.hypot(at[0] - last[0], at[1] - last[1]) * scale >= 2) this.dragPolygon = [...this.dragPolygon, at];
    } else {
      const [sx, sy] = this.dragStart;
      this.drag = [Math.min(sx, at[0]), Math.min(sy, at[1]), Math.max(sx, at[0]), Math.max(sy, at[1])];
    }
    e.stopPropagation();
  };

  private onPointerUp = (e: PointerEvent): void => {
    if (!this.dragStart) return;
    const box = this.drag;
    const polygon = this.dragPolygon;
    this.dragStart = null;
    this.drag = null;
    this.dragPolygon = null;
    e.stopPropagation();
    const s = this.resolvedStore;
    if (!s || !s.get('meta')) return;
    if (polygon) {
      // Fewer than three vertices is a click, not a shape.
      if (polygon.length < 3) return;
      this.select({kind: 'lasso', points: polygon.map(([x, y]) => s.dataXY(x, y))});
      return;
    }
    if (!box) return;
    // A click-sized box is not a selection.
    if (box[2] - box[0] < 1e-6 && box[3] - box[1] < 1e-6) return;
    const [x0, y0] = s.dataXY(box[0], box[1]);
    const [x1, y1] = s.dataXY(box[2], box[3]);
    const shape: SelectionShape = {kind: 'box', bbox: [x0, y0, x1, y1]};
    this.select(shape);
  };

  /** Select a shape programmatically, in data coordinates; `null` clears. */
  select(shape: SelectionShape | null): void {
    this.regionAskedAt = performance.now();
    if (this.probe.region) this.probe.region = null;
    this.resolvedStore?.select(shape);
    emit(this, 'tessera-selectchange', {shape, status: shape ? 'loading' : 'cleared'});
  }

  // ---- the camera -----------------------------------------------------------------------------

  private setViewState(next: Partial<ViewState>): void {
    this.viewState = {...this.viewState, ...next};
    this.deck?.setProps({viewState: this.viewState});
    this.pushView();
  }

  /** Fit the whole extent. */
  fit(): void {
    const {width, height} = this.size;
    this.setViewState({target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0], zoom: Math.log2(Math.min(width, height) / WORLD_SIZE)});
  }

  /** Fit an artifact's box, if the store holds one for it. */
  fitTo(artifactId: bigint): boolean {
    const s = this.resolvedStore;
    const extent = s?.extentOf(artifactId);
    const meta = s?.get('meta');
    if (!extent || !meta) return false;
    const q = meta.quantisation;
    const [x0, y0] = dataToWorldXY(extent[0], extent[1], q);
    const [x1, y1] = dataToWorldXY(extent[2], extent[3], q);
    const {width, height} = this.size;
    const bw = Math.abs(x1 - x0) || 1;
    const bh = Math.abs(y1 - y0) || 1;
    this.setViewState({
      target: [(x0 + x1) / 2, (y0 + y1) / 2, 0],
      zoom: Math.min(MAX_DEPTH, Math.log2(Math.min(width / bw, height / bh)) - 0.3)
    });
    return true;
  }

  private onKey = (e: KeyboardEvent): void => {
    if (e.target !== this) return;
    const step = 64 / 2 ** this.viewState.zoom;
    const [x, y] = this.viewState.target;
    switch (e.key) {
      case 'ArrowLeft':
        this.setViewState({target: [x - step, y, 0]});
        break;
      case 'ArrowRight':
        this.setViewState({target: [x + step, y, 0]});
        break;
      case 'ArrowUp':
        this.setViewState({target: [x, y - step, 0]});
        break;
      case 'ArrowDown':
        this.setViewState({target: [x, y + step, 0]});
        break;
      case '+':
      case '=':
        this.setViewState({zoom: Math.min(MAX_DEPTH, this.viewState.zoom + 1)});
        break;
      case '-':
      case '_':
        this.setViewState({zoom: Math.max(-2, this.viewState.zoom - 1)});
        break;
      case 'Escape':
        if (this.drag || this.dragPolygon) {
          this.dragStart = null;
          this.drag = null;
          this.dragPolygon = null;
        } else if (this.resolvedStore?.get('region')) {
          this.select(null);
        }
        break;
      default:
        return;
    }
    e.preventDefault();
  };

  // ---- the frame loop, for the probe ---------------------------------------------------------

  private startFrameLoop(): void {
    if (this.frameLoop !== null || typeof requestAnimationFrame === 'undefined') return;
    this.lastFrameAt = performance.now();
    const tick = () => {
      const now = performance.now();
      this.frameGaps.push(now - this.lastFrameAt);
      this.lastFrameAt = now;
      if (this.frameGaps.length > 120) this.frameGaps.shift();
      const sorted = [...this.frameGaps].sort((a, b) => a - b);
      this.probe.timings.frame = {
        mean: sorted.reduce((a, b) => a + b, 0) / (sorted.length || 1),
        p95: sorted[Math.floor(sorted.length * 0.95)] ?? 0,
        n: sorted.length
      };
      this.frameLoop = requestAnimationFrame(tick);
    };
    this.frameLoop = requestAnimationFrame(tick);
  }

  private stopFrameLoop(): void {
    if (this.frameLoop !== null && typeof cancelAnimationFrame !== 'undefined') cancelAnimationFrame(this.frameLoop);
    this.frameLoop = null;
  }

  // ---- render ---------------------------------------------------------------------------------

  override render() {
    const status = this.resolvedStore?.get('status') ?? null;
    const state: PanelState = stateOf(status);
    const overlay = state === 'shown' || state === 'stale' || state === 'detached' ? nothing : html`<div part="overlay">${renderState(state, status, {onRefresh: () => this.resolvedStore?.refresh()})}</div>`;
    return html`<div
        part="canvas"
        @pointerdown=${this.onPointerDown}
        @pointermove=${this.onPointerMove}
        @pointerup=${this.onPointerUp}
        @pointercancel=${this.onPointerUp}
      ></div>
      ${state === 'stale' ? html`<div part="overlay" style="align-items:flex-start;justify-content:center">${renderState(state, status, {onRefresh: () => this.resolvedStore?.refresh()})}</div>` : overlay}
      <div class="corner top-left">
        ${this.noControls
          ? nothing
          : html`<div part="controls" role="toolbar" aria-label="map mode">
              <button type="button" aria-pressed=${this.mode === 'pan'} title="pan (shift-drag selects)" @click=${() => (this.mode = 'pan')}>pan</button>
              <button type="button" aria-pressed=${this.mode === 'box'} title="drag a box to select" @click=${() => (this.mode = 'box')}>box</button>
              <button type="button" aria-pressed=${this.mode === 'lasso'} title="draw a shape to select" @click=${() => (this.mode = 'lasso')}>lasso</button>
              <button type="button" title="fit the whole extent" @click=${() => this.fit()}>fit</button>
            </div>`}
        <slot name="top-left"></slot>
      </div>
      <div class="corner top-right"><slot name="top-right"></slot></div>
      <div class="corner bottom-left"><slot name="bottom-left"></slot></div>
      <div class="corner bottom-right"><slot name="bottom-right"></slot></div>
      ${this.hover
        ? html`<div part="tooltip" style=${`left:${this.hover.x}px;top:${this.hover.y}px`}>
            <slot name="tooltip">${this.hover.lines.map((l) => html`<div>${l}</div>`)}</slot>
          </div>`
        : nothing}`;
  }
}

/** Compositions whose fidelity check has run — once per frame object. */
const checkedCompositions = new WeakSet<object>();

attachContextRoot();
defineOnce('tessera-map', TesseraMap);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-map': TesseraMap;
  }
}
