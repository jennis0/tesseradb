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
import {LookupTexture, MarkSlab, TesseraLayer, artifactOfMark, clusterLayerOf, contourShapes, encodingOf, encodingSignature, hoverAt, resolvePick, type ContourShape, type Picked} from '@tesseradb/deck';
import type {PaletteKind, PaletteScheme, Quantisation} from '@tesseradb/client';
import {TesseraElement, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import type {PickOutcome} from './item-card.js';
import {renderState, stateOf, type PanelState} from './states.js';
import {icon} from './icons.js';
import {sameFrame} from './view-switch.js';
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
 * and the settled shape goes to `store.select`, which puts it on every request as the `region`
 * leaf — the map narrows to it and its count is exact for the shape (`selection-operand.md`).
 *
 * **Colour by cluster** is `colour-by="cluster:<layer>"` (§5.10): the map owns the lookup texture
 * beside the slab, and the `palette` property chooses positional or spread (decision 0099).
 *
 * `display: block` with its height from `--tessera-map-height`, because a custom element is
 * inline and heightless and deck sizes its canvas from its parent.
 */

/** How long after disconnection the `Deck` is finalised, unless the element reconnects. */
const FINALIZE_SETTLE_MS = 250;
/**
 * How long the pointer must rest on a mark before its record is asked for — long enough that
 * crossing a dense map asks for nothing, short enough that stopping on a point answers before the
 * hand has settled.
 */
const HOVER_DESCRIBE_MS = 140;

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
  region: {verdict: string; exact: boolean; visible: number | null; matched: number; held: number; status: string; ms: number | null} | null;
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
    /**
     * What the last paint drew of the artifacts: the **rings** the outline layer carries, the
     * **artifacts** they belong to — the hovered and the opened one, which is all that draws — and
     * the labels placed. The first two are in different units because a shape is a list of parts
     * (`artifact-shapes.md` §1): opening a cluster whose members are two separated clouds hands
     * deck two parts and draws one shape. Neither counts what may be hovered, which is the frontier
     * and is held here rather than in the layer (`contours`).
     */
    outlines: number;
    outlinesDrawn: number;
    labels: number;
    /** The mark style the last paint drew — radius in pixels and composited alpha — and the resident count it was chosen for. */
    markRadius: number;
    markAlpha: number;
    markCount: number;
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
    /**
     * How many of the sampled ordinals the lookup texture draws in a colour — resolved against
     * the colours, which cover every artifact the session table holds, not only the served set
     * (§5.10). `resolvedId` answers a narrower question: which *served* artifact the ordinal
     * opens to. The two differ exactly where a band is held under a cut the view has moved off.
     */
    coloured: number;
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
        flex-direction: column;
        background: var(--tessera-surface);
        border: 1px solid var(--tessera-line);
        border-radius: var(--tessera-radius);
        box-shadow: var(--tessera-shadow);
        overflow: hidden;
        pointer-events: auto;
      }
      [part='controls'] button {
        width: 36px;
        height: 36px;
        display: grid;
        place-items: center;
        color: var(--tessera-ink-2);
        border-bottom: 1px solid var(--tessera-line-2);
        border-radius: 0;
      }
      [part='controls'] button:last-child {
        border-bottom: 0;
      }
      [part='controls'] button[aria-pressed='true'] {
        background: var(--tessera-accent-soft);
        color: var(--tessera-accent);
      }
      [part='controls'] .sep {
        height: 6px;
        background: var(--tessera-surface-2);
        border-bottom: 1px solid var(--tessera-line-2);
      }
      [part='tooltip'] {
        position: absolute;
        z-index: 4;
        pointer-events: none;
        padding: 8px 10px;
        max-width: 260px;
        background: var(--tessera-surface);
        border: 1px solid var(--tessera-line);
        border-radius: var(--tessera-radius);
        box-shadow: var(--tessera-shadow);
        font-size: 12px;
        transform: translate(14px, 14px);
      }
      [part='tooltip'] .t {
        font-weight: 500;
        line-height: 1.35;
      }
      [part='tooltip'] .s {
        margin-top: 3px;
        font-size: 11px;
        color: var(--tessera-ink-2);
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
        padding: 10px 14px;
        background: var(--tessera-surface);
        border: 1px solid var(--tessera-line);
        border-radius: var(--tessera-radius);
        box-shadow: var(--tessera-shadow);
        font-size: 13px;
        font-weight: 600;
      }
      [part='overlay'] [part='state'][data-state='refused'],
      [part='overlay'] [part='state'][data-state='expired'] {
        background: var(--tessera-refuse-soft);
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
  /**
   * The ground the map itself draws on, where that is not the host page's.
   *
   * **A light raster basemap under a dark page is what this exists for.** The label ink, its halo
   * and the positional palette's lightness answer to what is actually behind them, and the chrome
   * around the canvas answers to the page — one host can want both, and inferring either from the
   * other draws white names haloed in black over a pale street map. Unset, which is the ordinary
   * case, the host's `color-scheme` decides both.
   */
  @property({reflect: true}) accessor ground: 'light' | 'dark' | '' = '';
  /**
   * Whether the single-hue density wash is drawn under the points. **Off by default** (owner
   * direction, 2026-08-26): how density should be rendered is its own conversation, and the wash
   * was confounding a pass over the map's hierarchy. The machinery is untouched — `wash` turns it
   * on and the layer still builds it from the exact tiles' counts (decision 0097).
   */
  @property({type: Boolean}) accessor wash = false;
  /** A fixed mark radius in pixels; unset, the marks are sized by their count and the zoom (`markStyle`). */
  @property({type: Number}) accessor radius: number | null = null;
  /** The mode and fit control cluster — the map's own, not a slot. */
  @property({type: Boolean, attribute: 'no-controls'}) accessor noControls = false;
  /** Which corner the toolbar sits in: top-left docked, top-right overlay (the boards). */
  @property({attribute: 'controls-corner'}) accessor controlsCorner: 'top-left' | 'top-right' = 'top-left';

  @state() accessor hover: {x: number; y: number; title: string; lines: string[]} | null = null;
  /** The artifact under the pointer — its outline, its label, or a mark it holds — for the outline's highlight. */
  @state() accessor hoveredArtifact: bigint | null = null;
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
    timings: {slabMs: 0, washMs: 0, lutMs: 0, outlinesMs: 0, labelsMs: 0, layersMs: 0, lutWrites: 0, outlines: 0, outlinesDrawn: 0, labels: 0, markRadius: 0, markAlpha: 0, markCount: 0, frame: {mean: 0, p95: 0, n: 0}, decodeMs: []},
    cluster: {layer: null, layersOn: [], coverage: {current: 0, stale: 0}, servedIds: [], sample: [], coloured: 0}
  };

  readonly slab = new MarkSlab();
  readonly lut = new LookupTexture();
  private deck: Deck<OrthographicView> | null = null;
  private finalizeTimer: ReturnType<typeof setTimeout> | null = null;
  /** The pending hover-record request's dwell, cancelled by the next hover and by disconnection. */
  private describeTimer: ReturnType<typeof setTimeout> | null = null;
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
  private dragPointer: number | null = null;
  private metaSeen = false;
  /**
   * The frame the camera was last fitted or scheduled under (`view-switching.md` §4) — what a
   * switch is compared against to decide whether the camera moves.
   */
  private cameraFrame: Quantisation | null = null;
  /** The view last drawn for; a change in it is a switch, and a switch drops the hover. */
  private cameraView = '';
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
    if (this.describeTimer !== null) {
      clearTimeout(this.describeTimer);
      this.describeTimer = null;
    }
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
      // The ground decides the positional palette's lightness as well as the labels', so a ground
      // the host declared after the store was adopted has to reach it here.
      if (changed.has('ground')) s.setScheme(this.scheme());
    }
    if (changed.has('mode') || changed.has('drag') || changed.has('dragPolygon') || changed.has('basemap') || changed.has('ground') || changed.has('wash') || changed.has('radius') || changed.has('clusterLevel') || changed.has('hoveredArtifact')) this.paint();
  }

  /**
   * The ground the map draws on, from the host's `color-scheme`: `dark` or `light` as declared,
   * else the system preference. The positional palette's lightness follows it (§5.10).
   */
  private scheme(): PaletteScheme {
    if (this.ground === 'light' || this.ground === 'dark') return this.ground;
    if (typeof getComputedStyle === 'undefined') return 'dark';
    const declared = getComputedStyle(this).colorScheme ?? '';
    const dark = /dark/.test(declared);
    const light = /light/.test(declared);
    if (dark && !light) return 'dark';
    if (light && !dark) return 'light';
    return typeof matchMedia !== 'undefined' && matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }

  protected override onStoreAdopted(store: Store): void {
    this.slab.clear();
    store.setScheme(this.scheme());
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
    // **Every switch drops the hover** (owner ruling, 2026-09-01), a switch within a group
    // included: the marks under the cursor are different rows in the next view, and a tooltip
    // held across the step would name a record that is no longer beneath the pointer.
    if (view.id !== this.cameraView) {
      this.cameraView = view.id;
      this.hover = null;
      this.hoveredArtifact = null;
    }
    // **A switch across frames refits; a switch within a group does not** (`view-switching.md`
    // §4). Every view of a group shares one frame, so the same tiles at the same depth are the
    // request in the next view and the store re-schedules the camera itself — moving the camera
    // would throw away the position the user is reading.
    //
    // The question is asked of the **frame**, on every tick and not under a `view.id` gate: a pan
    // has already written `cameraFrame` through `pushView`, so the two agree, and the refit does
    // not depend on the store publishing the new id and the new extent in one tick. A null
    // `cameraFrame` is the interval before the first camera went out, where there is nothing to
    // refit from and the initial view is drawn as it is.
    const frame = s.frame();
    if (frame && this.cameraFrame && !sameFrame(frame, this.cameraFrame)) this.fit();
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
    // The store's own view's frame (decision 0040): the extent is the view's, so the conversion
    // between data coordinates and world space is asked of the store rather than read off the
    // bundle, which no longer has one.
    const q = s.frame();
    if (region && q) {
      if (region.shape.kind === 'box') {
        const [x0, y0] = dataToWorldXY(region.shape.bbox[0], region.shape.bbox[1], q);
        const [x1, y1] = dataToWorldXY(region.shape.bbox[2], region.shape.bbox[3], q);
        const next: [number, number, number, number] = [Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)];
        if (!this.regionWorld || next.some((v, i) => v !== this.regionWorld![i])) {
          this.regionWorld = next;
          this.regionPolygon = null;
          this.paint();
        }
      } else if (region.shape.kind === 'lasso') {
        if (region.shape !== this.regionShape) {
          this.regionShape = region.shape;
          this.regionWorld = null;
          this.regionPolygon = region.shape.points.map(([x, y]) => dataToWorldXY(x, y, q));
          this.paint();
        }
      } else if (region.shape !== this.regionShape) {
        // An artifact selection draws no shape of its own: the map narrows to its members, and
        // the opened artifact's outline is the drawing (`polygon-membership.md` §8).
        this.regionShape = region.shape;
        this.regionWorld = null;
        this.regionPolygon = null;
        this.paint();
      }
      const ms = region.status === 'loading' ? null : (p.region?.ms ?? performance.now() - this.regionAskedAt);
      const verdict = region.verdict === null ? 'pending' : region.verdict.exact ? 'exact' : `cover; depth=${region.verdict.depth}`;
      p.region = {verdict, exact: region.matched.exact, visible: region.visible?.value ?? null, matched: region.matched.value, held: region.held.count, status: region.status, ms};
      if (region.status !== 'loading' && region !== this.regionAnnounced) {
        this.regionAnnounced = region;
        emit(this, 'tessera-selectchange', {
          shape: region.shape,
          status: region.status,
          visible: region.visible,
          matched: region.matched,
          served: region.served,
          verdict: region.verdict
        });
      }
    } else if (!region && (this.regionWorld || this.regionPolygon)) {
      this.regionWorld = null;
      this.regionPolygon = null;
      this.regionShape = null;
      p.region = null;
      this.paint();
    }
    // The opened artifact is a property of the layer, read at a paint: an open or a close
    // repaints so the outline highlights (the layer's own subscription redraws the projections,
    // not the properties the host computes).
    const opened = sel.artifact?.id ?? null;
    if (opened !== this.paintedOpened) {
      this.paintedOpened = opened;
      this.paint();
    }
    super.onStoreChange();
  }

  private probedArtifacts: object | null = null;
  private probedComposition: object | null = null;
  /** The opened artifact the last paint drew, so a change repaints once. */
  private paintedOpened: bigint | null = null;
  private regionShape: SelectionShape | null = null;

  /** See {@link MapProbe.cluster}: a sample of carried ordinals, each resolved through the table. */
  private clusterProbe(clusterLayer: string | null, artifacts: ReturnType<Store['get']> & {layers: string[]}, bands: readonly {membership: Record<string, {distinct: Uint32Array}>}[]): MapProbe['cluster'] {
    const a = artifacts as unknown as import('@tesseradb/client').ArtifactsProjection;
    const layer = clusterLayer ?? a.layers[0] ?? null;
    const sample: {ordinal: number; resolvedId: string | null}[] = [];
    let coloured = 0;
    if (layer) {
      for (const band of bands) {
        const m = band.membership[layer];
        if (!m) continue;
        for (let i = 0; i < m.distinct.length && sample.length < 16; i++) {
          const ordinal = m.distinct[i]!;
          const resolved = a.table.resolve(ordinal, a.servedOrdinals, this.clusterLevel ?? undefined);
          const entry = resolved === 0 ? null : a.table.entry(resolved);
          if (a.table.resolve(ordinal, a.colours, this.clusterLevel ?? undefined) !== 0) coloured += 1;
          sample.push({ordinal, resolvedId: entry ? idString(entry.tesseraId) : null});
        }
        if (sample.length >= 16) break;
      }
    }
    return {layer, layersOn: a.layers, coverage: a.coverage, servedIds: a.served.map((x) => idString(x.tesseraId)), sample, coloured};
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
    this.cameraFrame = s.frame();
    this.cameraView = s.get('view').id;
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
          hoveredArtifact: this.hoveredArtifact,
          region: this.regionWorld,
          regionPolygon: this.regionPolygon,
          drag: this.drag,
          dragPolygon: this.dragPolygon,
          wash: this.wash,
          radius: this.radius,
          scheme: this.scheme(),
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
              lutWrites: t.lutWrites,
              outlinesDrawn: t.outlinesDrawn,
              outlines: t.outlines,
              labels: t.labels,
              markRadius: t.markRadius,
              markAlpha: t.markAlpha,
              markCount: t.markCount
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

  /**
   * The shapes a hover — and a click — is resolved against, held until the served set, the fetched
   * shapes, the level or the roster move. They are the **served** rings and never the drawn curve:
   * the curve is a smoothing, and an answer given against it would be about a line the wire never
   * sent. They answer *which artifact is under the pointer* and never whether a point is a member
   * — that is the wire's membership column (`polygon-membership.md` §7.1).
   *
   * **A shape here is the artifact's `box` until its served shape arrives.** The viewport is asked
   * for centroids and boxes, so at rest every candidate is a rectangle and the hover is coarser
   * than it was: two clusters whose boxes overlap are separated by depth and by the mark under
   * the pointer (`hoverAt`'s `prefer`), which is the wire's own membership and a better answer
   * than geometry gave. The shape for whatever the hover lands on is fetched immediately, and the
   * index is rebuilt around it when it lands.
   */
  private contoursHeld: {served: object; fetched: object; level: number | null; meta: object | null; shapes: ContourShape[]} | null = null;

  private contours(): ContourShape[] {
    const s = this.resolvedStore;
    const a = s?.get('artifacts') ?? null;
    if (!a) return [];
    const meta = s?.get('meta') ?? null;
    const level = this.clusterLevel ?? null;
    const held = this.contoursHeld;
    if (held && held.served === a.served && held.fetched === a.shapes && held.level === level && held.meta === meta) return held.shapes;
    const shapes = contourShapes(a, {level: level ?? undefined, meta});
    this.contoursHeld = {served: a.served, fetched: a.shapes, level, meta, shapes};
    return shapes;
  }

  /** How far past a hovered shape's edge the pointer goes before the hover lets go, in pixels. */
  private static readonly HOVER_MARGIN_PX = 4;

  /**
   * The artifact a world point is over, for the hover and for the click alike: the deepest drawn
   * shape containing it, the one already hovered held until the pointer is clear of it, and the
   * mark beneath preferred where the caller has one ({@link hoverAt}).
   *
   * One route, so that what a click opens is what the pointer was highlighting. Nothing else can
   * answer it: the outline layer draws the hovered and the opened artifact only and is not
   * pickable, so a contour never reaches deck's pick pass.
   */
  private artifactAt(world: [number, number], prefer: bigint | null): bigint | null {
    return hoverAt(this.contours(), world, this.hoveredArtifact, TesseraMap.HOVER_MARGIN_PX / 2 ** this.viewState.zoom, prefer);
  }

  /**
   * What the pointer is over: the tooltip from the mark beneath it, and the hovered artifact from
   * the frontier's own shapes ({@link hoverAt}), never from deck's pick.
   *
   * Deck answers a pick with whatever polygon its picking pass finds, which was every served
   * artifact — ancestors included, at zero alpha — so crossing a cluster meant crossing its
   * parent's invisible ring too and the highlight flipped between the two on a pixel of movement
   * (the owner's review, 2026-08-27). The shapes are now the frontier's alone, the deepest one
   * containing the pointer wins, and the one already hovered holds until the pointer is clear of
   * it. The mark beneath the pointer is passed as the preference, so the highlighted contour is
   * the cluster whose point the tooltip is describing.
   *
   * Deck cannot answer it at all: the outline layer holds the hovered and the opened artifact and
   * is not pickable, so a contour is never in its pick pass.
   */
  private onHover(info: PickingInfo): void {
    const picked = resolvePick(info as never);
    const layerId = (info.sourceLayer ?? info.layer)?.id ?? '';
    const slot = picked.kind === 'mark' ? /marks-p(\d+)$/.exec(layerId) : null;
    const at = slot ? this.slab.markAt(Number(slot[1]), info.index) : null;
    const artifacts = this.resolvedStore?.get('artifacts') ?? null;
    // The mark's own artifact, where there is one — a fact the wire carries, and the tie-break
    // where two shapes interleave over the same ground.
    const own = at && artifacts ? artifactOfMark(at.band, at.i, artifacts, this.clusterLevel ?? undefined) : null;
    const world = this.worldAt(info.x, info.y);
    this.hoveredArtifact = world ? this.artifactAt(world, own) : null;
    // **The shape is fetched where it is drawn.** The viewport carries no shape; this asks for the
    // one the map is about to draw. Idempotent, so calling it on every pointer move costs one
    // request per artifact per principal and nothing thereafter.
    if (this.hoveredArtifact !== null) this.resolvedStore?.needShape(this.hoveredArtifact);
    if (picked.kind !== 'mark') {
      if (this.hover) this.hover = null;
      return;
    }
    // The hint: the first tooltip field as the title (a `title` column, typically), the rest as
    // one muted line beneath — the boards' `tooltip`. With no fields, the id.
    const values: string[] = [];
    const fields = this.tooltipFields.split(/[\s,]+/).filter(Boolean);
    if (fields.length > 0 && slot) {
      if (at) {
        for (const f of fields) {
          const column = at.band.scalars[f];
          if (!column) continue;
          const raw = (column.values as ArrayLike<unknown>)[at.i];
          values.push(column.arrowType === 'timestamp_us' ? new Date(Number(raw) / 1000).toISOString().slice(0, 4) : String(raw));
        }
      }
    }
    const title = values[0] ?? `#${idString(picked.id)}`;
    const lines = values.slice(1);
    this.hover = {x: info.x, y: info.y, title, lines};
    // Nothing the marks carry names the thing under the pointer, so ask the record — see
    // {@link describeHovered}. The id stands until the answer lands, and stands for good where
    // the corpus has no text column or the record cannot be reached.
    if (values.length === 0) this.describeHovered(picked.id, info.x, info.y);
    emit(this, 'tessera-hover', {id: idString(picked.id), x: info.x, y: info.y});
  }

  /**
   * The hovered point's own name, fetched once the pointer rests on it.
   *
   * **A text column cannot be drawn**, so it is not in the response the marks came from: prose
   * lives in the record blob and `render` on a text column is refused (records-and-search §3).
   * The id is therefore the whole of what a mark knows about itself, and a tooltip reading
   * `#31728047486770` is the honest rendering of that — and useless. One request per point, held
   * by the store, is what turns it into a name.
   *
   * **After a dwell, not on the move.** A pointer crossing a dense map touches hundreds of marks a
   * second and none of them is being looked at; {@link HOVER_DESCRIBE_MS} is the pause that says
   * one of them is. A pointer that has moved on by the time the answer lands writes nothing.
   */
  private describeHovered(id: bigint, x: number, y: number): void {
    const column = this.nameColumn();
    if (column === null) return;
    if (this.describeTimer !== null) clearTimeout(this.describeTimer);
    this.describeTimer = setTimeout(() => {
      this.describeTimer = null;
      const store = this.resolvedStore;
      if (!store) return;
      void store.describe(id).then((fields) => {
        const name = fields?.[column];
        if (typeof name !== 'string' || name === '') return;
        // The pointer may have left, or moved to another mark, while this was in flight.
        if (!this.hover || this.hover.x !== x || this.hover.y !== y) return;
        this.hover = {...this.hover, title: name};
      });
    }, HOVER_DESCRIBE_MS);
  }

  /**
   * The column a hover names a point by: the first text column the bundle declares, or `null`
   * where it declares none — every arXiv corpus answers `title`, this one answers `name`, and a
   * corpus of bare points answers nothing and keeps its ids.
   */
  private nameColumn(): string | null {
    const scalars = (this.resolvedStore?.get('meta') ?? null)?.declaredScalars ?? [];
    // Prose first — a corpus with both a `text` title and a `keyword` code means the title — then
    // whatever string column exists.
    return (
      scalars.find((d) => d.arrowType === 'text')?.name ??
      scalars.find((d) => d.arrowType === 'utf8' || d.arrowType === 'keyword')?.name ??
      null
    );
  }

  private onClick(info: PickingInfo): void {
    const s = this.resolvedStore;
    const picked: Picked = resolvePick(info as never);
    switch (picked.kind) {
      case 'artifact':
        this.lastPick = null;
        // The opened artifact draws its shape too, and the card's own request does not carry it
        // into the projection the map reads.
        s?.needShape(picked.id);
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
      case 'miss': {
        // Deck found nothing, which does not mean the pointer is over nothing: a contour is not in
        // its pick pass. Resolve the click exactly as the hover is resolved, so what opens is what
        // was highlighted, and fall back to a miss where the point is in no drawn shape.
        const world = this.worldAt(info.x, info.y);
        const id = world ? this.artifactAt(world, null) : null;
        if (id === null) {
          this.lastPick = {kind: 'miss'};
          return;
        }
        this.lastPick = null;
        s?.needShape(id);
        void s?.openArtifact(id);
        return;
      }
      case 'broken':
        this.lastPick = picked;
        return;
    }
  }

  // ---- selection: the box and the lasso -------------------------------------------------------

  private unproject(e: PointerEvent): [number, number] | null {
    const rect = this.getBoundingClientRect();
    return this.worldAt(e.clientX - rect.left, e.clientY - rect.top);
  }

  /** A point in canvas pixels to world space, through the live viewport. */
  private worldAt(x: number, y: number): [number, number] | null {
    const viewport = this.deck?.getViewports()[0];
    if (!viewport || !Number.isFinite(x) || !Number.isFinite(y)) return null;
    const xy = viewport.unproject([x, y]);
    return [xy[0]!, xy[1]!];
  }

  /**
   * The selection gestures are taken in the **capture phase, before deck's own input layer sees
   * them**. deck (mjolnir/hammer) listens for `pointerdown` on its canvas and for the move and
   * up on `window`; the old handlers ran on the canvas's parent in the bubble phase and stopped
   * propagation there, so hammer saw every selection's `pointerdown` and never its `pointerup`.
   * Its input session stayed pressed: the next mouse movement — button up, on the way to the
   * *pan* button — read as a drag, the camera followed the pointer once the controller was back
   * on, and the real pan drag that followed did nothing because hammer's session was already in
   * flight. Found with human-paced pointer input in a real browser; the fast synthetic drag did
   * not stay pressed long enough to show it. Stopping the `pointerdown` before it reaches the
   * canvas means hammer never opens a session for a selection, so there is nothing to close.
   */
  private onPointerDown = (e: PointerEvent): void => {
    if (e.button !== 0) return;
    const lasso = this.mode === 'lasso';
    if (!lasso && this.mode !== 'box' && !(this.mode === 'pan' && e.shiftKey)) return;
    const at = this.unproject(e);
    if (!at) return;
    this.dragStart = at;
    this.dragPointer = e.pointerId;
    if (lasso) this.dragPolygon = [at];
    else this.drag = [at[0], at[1], at[0], at[1]];
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    e.stopPropagation();
    e.preventDefault();
  };

  private onPointerMove = (e: PointerEvent): void => {
    if (!this.dragStart || e.pointerId !== this.dragPointer) return;
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
    if (!this.dragStart || e.pointerId !== this.dragPointer) return;
    const box = this.drag;
    const polygon = this.dragPolygon;
    this.dragStart = null;
    this.dragPointer = null;
    this.drag = null;
    this.dragPolygon = null;
    e.stopPropagation();
    // A cancel, or a capture lost to the platform, ends the gesture and selects nothing.
    if (e.type !== 'pointerup') return;
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

  /** The camera's zoom: 0 when the 512-unit world fills 512 px, +1 per doubling. */
  get zoom(): number {
    return this.viewState.zoom;
  }

  /** Fit the whole extent. */
  fit(): void {
    const {width, height} = this.size;
    this.setViewState({target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0], zoom: Math.log2(Math.min(width, height) / WORLD_SIZE)});
  }

  /**
   * Centre the camera on a **data coordinate** at the current zoom — what following an item into
   * another view asks for (`view-switching.md` §6.4). `false` before `meta`, when there is no
   * frame to convert against.
   */
  lookAt(x: number, y: number): boolean {
    const q = this.resolvedStore?.frame();
    if (!q) return false;
    const [wx, wy] = dataToWorldXY(x, y, q);
    this.setViewState({target: [wx, wy, 0]});
    return true;
  }

  /** Fit an artifact's box, if the store holds one for it. */
  fitTo(artifactId: bigint): boolean {
    const extent = this.resolvedStore?.extentOf(artifactId);
    if (!extent) return false;
    return this.fitBbox(extent);
  }

  /**
   * Fit a box given in **data coordinates** — the space `setView` takes and `tessera-viewchange`
   * reports, so a host (the notebook widget's `bbox`) can hand back what it was told. The camera
   * keeps the canvas's aspect, so the box shown contains the one asked for and the next
   * `tessera-viewchange` reports the box actually shown.
   */
  fitBbox(extent: [number, number, number, number]): boolean {
    const q = this.resolvedStore?.frame();
    if (!q) return false;
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
    // Loading and retrying are the strip's to say; the map draws only what must never read as an
    // empty corpus: a refusal, an expiry, an empty answer.
    const overlay = state === 'refused' || state === 'expired' || state === 'empty' ? html`<div part="overlay">${renderState(state, status, {onRefresh: () => this.resolvedStore?.refresh()})}</div>` : nothing;
    const controls = this.noControls
      ? nothing
      : html`<div part="controls" role="toolbar" aria-label="Map tools">
          <button type="button" aria-label="Pan" aria-pressed=${this.mode === 'pan'} title="Pan (shift-drag selects)" @click=${() => (this.mode = 'pan')}>${icon('pan')}</button>
          <button type="button" aria-label="Box select" aria-pressed=${this.mode === 'box'} title="Box select" @click=${() => (this.mode = 'box')}>${icon('box')}</button>
          <button type="button" aria-label="Lasso select" aria-pressed=${this.mode === 'lasso'} title="Lasso select" @click=${() => (this.mode = 'lasso')}>${icon('lasso')}</button>
          <div class="sep"></div>
          <button type="button" aria-label="Fit to extent" title="Fit to extent" @click=${() => this.fit()}>${icon('fit')}</button>
        </div>`;
    return html`<div
        part="canvas"
        @pointerleave=${() => {
          // deck only reports picks while the pointer is over it; a hover left standing when the
          // pointer moves onto a panel is a box beside nothing.
          if (this.hover) this.hover = null;
        }}
        @pointerdown=${{handleEvent: this.onPointerDown, capture: true}}
        @pointermove=${{handleEvent: this.onPointerMove, capture: true}}
        @pointerup=${{handleEvent: this.onPointerUp, capture: true}}
        @pointercancel=${{handleEvent: this.onPointerUp, capture: true}}
        @lostpointercapture=${{handleEvent: this.onPointerUp, capture: true}}
      ></div>
      ${overlay}
      <div class="corner top-left">
        ${this.controlsCorner === 'top-left' ? controls : nothing}
        <slot name="top-left"></slot>
      </div>
      <div class="corner top-right">
        ${this.controlsCorner === 'top-right' ? controls : nothing}
        <slot name="top-right"></slot>
      </div>
      <div class="corner bottom-left"><slot name="bottom-left"></slot></div>
      <div class="corner bottom-right"><slot name="bottom-right"></slot></div>
      ${this.hover
        ? html`<div part="tooltip" style=${`left:${this.hover.x}px;top:${this.hover.y}px`}>
            <slot name="tooltip"><div class="t">${this.hover.title}</div>${this.hover.lines.length > 0 ? html`<div class="s">${this.hover.lines.join(' · ')}</div>` : nothing}</slot>
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
