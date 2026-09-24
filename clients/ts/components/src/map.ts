import {Deck, OrthographicView, type Layer, type PickingInfo} from '@deck.gl/core';
import {css, html, nothing, type PropertyValues} from 'lit';
import {property, state} from 'lit/decorators.js';
import {
  MAX_DEPTH,
  WORLD_SIZE,
  assertCompositionMatchesServed,
  dataToWorldXY,
  hasValue,
  type ArtifactsProjection,
  type FiltersProjection,
  type MarksProjection,
  type RegionProjection,
  type Store,
  type SelectionShape,
  type ViewProjection
} from '@tesseradb/client';
import {TesseraLayer, resolvePick, viewInputOf, type Picked} from '@tesseradb/deck';
import {MarkSlab, artifactOfMark, clusterLayerOf, contourShapes, encodingOf, encodingSignature, hoverAt, type ContourShape} from '@tesseradb/deck/internal';
import type {PaletteKind, PaletteScheme, Quantisation} from '@tesseradb/client';
import {TesseraElement, emit, idString, shapeDetail} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {timestampText, type PickOutcome} from './item-card.js';
import {renderState, stateOf, type PanelState} from './states.js';
import {icon} from './icons.js';
import {sameFrame} from './view-switch.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-map>` is the canvas: points, the density wash, the artifact outlines and labels,
 * hover, pick, the selection, and the refused, expired and empty states drawn over the canvas, so
 * none of them reads as an empty corpus.
 *
 * Each map owns its `Deck`, its `MarkSlab` and its probe. The `Deck` is finalised a moment after
 * disconnection, and not at all if the element reconnects first, since notebooks and frameworks
 * disconnect and reconnect elements routinely and the GPU slab would be rebuilt each time.
 *
 * The map owns the camera: deck's world-space `{target, zoom}` goes to `setView` through
 * `viewInputOf` on every change, and the store debounces. `fit()`, `fitTo()` and the keyboard move
 * the view state the element holds.
 *
 * `mode="box"`, or shift-drag in `pan`, draws a box; `mode="lasso"` a freehand polygon. The
 * settled shape goes to `store.select`, which sends it on every request as the `region` leaf, so
 * the map narrows to it and its count is exact for the shape.
 *
 * `colour-by="cluster:<layer>"` colours by cluster, with `palette` choosing positional or spread.
 *
 * The host is `display: block` with its height from `--tessera-map-height`, since deck sizes its
 * canvas from its parent.
 */

/** How long after disconnection the `Deck` is finalised, unless the element reconnects. */
const FINALIZE_SETTLE_MS = 250;
/** How long the pointer must rest on a mark before its record is asked for. */
const HOVER_DESCRIBE_MS = 140;

/**
 * What the map reports about itself for instruments and tests, one object mutated in place.
 * `timings.frame` and `cluster` are filled only while `measure` is on.
 */
export type MapProbe = {
  paints: number;
  at: number;
  marks: number;
  /** Viewport requests the store has issued, when its instruments report them; else 0. */
  requests: number;
  encoding: string;
  view: {depth: number; status: string; stale: boolean; visible: number; matched: number; served: number; provisional: number};
  /** `ms` is from the selection to its counts arriving, on the store's clock. */
  region: {verdict: string; exact: boolean; visible: number | null; matched: number; held: number; status: string; ms: number | null} | null;
  timings: {
    /** The last paint's work per stage, in milliseconds. */
    slabMs: number;
    washMs: number;
    lutMs: number;
    outlinesMs: number;
    labelsMs: number;
    layersMs: number;
    /** Writes to the layer's lookup texture since the layer made it. */
    lutWrites: number;
    /**
     * What the last paint drew: the outline parts, the artifacts they belong to (the hovered and
     * the opened), and the labels placed. An artifact in several pieces has several parts.
     */
    outlines: number;
    outlinesDrawn: number;
    labels: number;
    /** The mark radius in pixels and alpha the last paint drew, and the resident count behind them. */
    markRadius: number;
    markAlpha: number;
    markCount: number;
    /** Frame gaps over the last two seconds, in milliseconds. */
    frame: {mean: number; p95: number; n: number};
    /** Per-response decode, reported by the host through the store's instruments. */
    decodeMs: number[];
  };
  /**
   * Colour by cluster: the layer coloured by (else the first drawn), its served ids, and a sample
   * of the marks' ordinals with the served artifact each resolves to, so a test can check that a
   * coloured mark resolves to a served artifact. `layersOn` is the layers drawn.
   */
  cluster: {
    layer: string | null;
    layersOn: string[];
    coverage: {current: number; stale: number};
    servedIds: string[];
    sample: {ordinal: number; resolvedId: string | null}[];
    /**
     * How many sampled ordinals the lookup texture colours. Colours cover every artifact in the
     * session table, so this can exceed the ordinals with a served `resolvedId`, where a band is
     * held under a cut the view has moved off.
     */
    coloured: number;
  };
  [extra: string]: unknown;
};

type ViewState = {target: [number, number, number]; zoom: number; minZoom: number; maxZoom: number};

const VIEW = new OrthographicView({id: 'ortho', flipY: true});

/**
 * Which count the density wash reads, and so its label: `highlighted` under a highlight, `matched`
 * when the request's `filters` carries anything (the filter clauses, `member_of` clauses and the
 * drawn region), `visible` otherwise. The choice follows what was asked, since the counts are
 * equal when nothing narrows them.
 */
export function washChannel(
  filters: FiltersProjection,
  view: ViewProjection,
  region: RegionProjection | null
): 'visible' | 'matched' | 'highlighted' {
  if (view.highlighting) return 'highlighted';
  const filtering = filters.expr !== null || filters.members.some((c) => c.verb === 'filter') || region !== null;
  return filtering ? 'matched' : 'visible';
}

export class TesseraMap extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
        position: relative;
        height: var(--_tessera-map-height);
        min-height: 120px;
        background: var(--_tessera-map-bg);
        outline: none;
        overflow: hidden;
      }
      :host(:focus-visible) {
        box-shadow: inset 0 0 0 2px var(--_tessera-accent);
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
        gap: var(--_tessera-space);
        max-width: 46%;
        pointer-events: none;
      }
      .corner > ::slotted(*) {
        pointer-events: auto;
      }
      .top-left {
        top: var(--_tessera-space);
        left: var(--_tessera-space);
      }
      .top-right {
        top: var(--_tessera-space);
        right: var(--_tessera-space);
      }
      .bottom-left {
        bottom: var(--_tessera-space);
        left: var(--_tessera-space);
      }
      .bottom-right {
        bottom: var(--_tessera-space);
        right: var(--_tessera-space);
        align-items: flex-end;
      }
      [part='controls'] {
        display: flex;
        flex-direction: column;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
        overflow: hidden;
        pointer-events: auto;
      }
      [part='controls'] button {
        width: 36px;
        height: 36px;
        display: grid;
        place-items: center;
        color: var(--_tessera-ink-2);
        border-bottom: 1px solid var(--_tessera-line-2);
        border-radius: 0;
      }
      [part='controls'] button:last-child {
        border-bottom: 0;
      }
      [part='controls'] button[aria-pressed='true'] {
        background: var(--_tessera-accent-soft);
        color: var(--_tessera-accent);
      }
      [part='controls'] .sep {
        height: 6px;
        background: var(--_tessera-surface-2);
        border-bottom: 1px solid var(--_tessera-line-2);
      }
      [part='tooltip'] {
        position: absolute;
        z-index: 4;
        pointer-events: none;
        padding: 8px 10px;
        max-width: 260px;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
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
        color: var(--_tessera-ink-2);
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
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
        font-size: 13px;
        font-weight: 600;
      }
      [part='overlay'] [part='state'][data-state='refused'],
      [part='overlay'] [part='state'][data-state='expired'] {
        background: var(--_tessera-refuse-soft);
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
  /** Columns the marks carry, shown beneath a hovered point's title. */
  @property({attribute: 'tooltip-fields'}) accessor tooltipFields = '';
  /**
   * The record field a hovered point is titled by, read from the marks where they carry it, else
   * from the record once the pointer rests. Unset, the title is the point's id.
   */
  @property({attribute: 'title-field'}) accessor titleField = '';
  @property({reflect: true}) accessor mode: 'pan' | 'box' | 'lasso' = 'pan';
  /** How served artifacts are coloured: by position about the extent's centre, or spread over the served set. */
  @property() accessor palette: PaletteKind = 'positional';
  /** The level to colour a nested layer at; unset colours at the deepest served. */
  @property({type: Number, attribute: 'cluster-level'}) accessor clusterLevel: number | null = null;
  /** A deck.gl layer drawn under the points, such as a basemap. */
  @property({attribute: false}) accessor basemap: Layer | null = null;
  /**
   * The ground the map draws on, where it differs from the page's, such as a light basemap under a
   * dark page. The label ink, halo and positional palette follow it; the chrome follows the page.
   * Unset, the host's `color-scheme` decides both.
   */
  @property({reflect: true}) accessor ground: 'light' | 'dark' | '' = '';
  /** Whether the single-hue density wash, built from the exact tiles' counts, is drawn under the points. */
  @property({type: Boolean}) accessor wash = false;
  /**
   * Whether the map measures itself for the probe: the frame-gap loop behind `probe.timings.frame`,
   * the colour-by-cluster sample behind `probe.cluster`, and the check that each composition
   * matches what was served. Off, none of the three runs.
   */
  @property({type: Boolean}) accessor measure = false;
  /** A fixed mark radius in pixels; unset, the marks are sized by their count and the zoom (`markStyle`). */
  @property({type: Number}) accessor radius: number | null = null;
  /** Hides the mode and fit controls. */
  @property({type: Boolean, attribute: 'no-controls'}) accessor noControls = false;
  /** Which corner the controls sit in. */
  @property({attribute: 'controls-corner'}) accessor controlsCorner: 'top-left' | 'top-right' = 'top-left';

  @state() accessor hover: {x: number; y: number; title: string; lines: string[]} | null = null;
  /** The artifact under the pointer (its outline, label or a mark it holds), whose outline is drawn. */
  @state() accessor hoveredArtifact: bigint | null = null;
  @state() accessor drag: [number, number, number, number] | null = null;
  @state() accessor dragPolygon: [number, number][] | null = null;

  /** What the last click resolved to: a miss, a broken pick, or a hit sent to the store. */
  lastPick: PickOutcome = null;
  /** The map's probe, one object mutated in place. */
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

  /** Passed to the layer, so a hover can read a mark's band from it. */
  readonly slab = new MarkSlab();
  private deck: Deck<OrthographicView> | null = null;
  private finalizeTimer: ReturnType<typeof setTimeout> | null = null;
  /** The pending hover dwell, cancelled by the next hover and by disconnection. */
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
  /** The frame the camera was last fitted or scheduled under, compared on a switch to decide whether to refit. */
  private cameraFrame: Quantisation | null = null;
  /** The view last drawn for; a change in it is a switch, and a switch drops the hover. */
  private cameraView = '';
  private frameGaps: number[] = [];
  private frameLoop: number | null = null;
  private lastFrameAt = 0;

  override connectedCallback(): void {
    super.connectedCallback();
    if (this.finalizeTimer) {
      clearTimeout(this.finalizeTimer);
      this.finalizeTimer = null;
    }
    if (!this.hasAttribute('tabindex')) this.tabIndex = 0;
    this.setAttribute('role', 'application');
    if (!this.hasAttribute('aria-label')) this.setAttribute('aria-label', 'map: arrow keys pan, + and - zoom');
    this.addEventListener('keydown', this.onKey);
    if (this.measure) this.startFrameLoop();
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
    if (changed.has('measure')) {
      if (this.measure && this.isConnected) this.startFrameLoop();
      else this.stopFrameLoop();
      if (this.measure && this.resolvedStore) this.measureStore(this.resolvedStore);
    }
    const s = this.resolvedStore;
    if (s) {
      if (changed.has('colourBy') && this.colourBy !== '') s.setColourBy(this.colourBy === 'none' ? null : this.colourBy);
      if (changed.has('layers') && this.layers) {
        s.setLayers(this.layers);
        emit(this, 'tessera-layerchange', {layers: this.layers});
      }
      if (changed.has('budget') && this.budget > 0) s.setBudget(this.budget);
      if (changed.has('palette')) s.setPalette(this.palette);
      // The ground also sets the positional palette's lightness in the store.
      if (changed.has('ground')) s.setScheme(this.scheme());
    }
    if (changed.has('mode') || changed.has('drag') || changed.has('dragPolygon') || changed.has('basemap') || changed.has('ground') || changed.has('wash') || changed.has('radius') || changed.has('clusterLevel') || changed.has('hoveredArtifact')) this.paint();
  }

  /** The ground: `ground` if set, else the host's `color-scheme`, else the system preference. */
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

  protected override onStoreAdopted(store: Store | null): void {
    this.slab.clear();
    this.metaSeen = false;
    this.selectedWorldXY = null;
    this.regionWorld = null;
    this.regionPolygon = null;
    if (!store) {
      this.paint();
      return;
    }
    store.setScheme(this.scheme());
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
    // Every switch drops the hover: the marks under the cursor are different rows in the new view.
    if (view.id !== this.cameraView) {
      this.cameraView = view.id;
      this.hover = null;
      this.hoveredArtifact = null;
    }
    // A switch to another frame refits; a switch within a group, whose views share a frame, keeps
    // the camera. Compared on every change rather than on a new view id, so the refit does not
    // need the id and extent to change together. Before the first camera, `cameraFrame` is null.
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
    if (this.measure) this.measureStore(s);

    // Events for what arrived: the picked record, the opened artifact, the region's counts.
    const sel = s.get('selection');
    if (sel.item && sel.item !== this.announcedItem) {
      this.announcedItem = sel.item;
      emit(this, 'tessera-pick', {id: idString(sel.item.id), record: sel.item.detail});
    }
    if (sel.artifact && sel.artifact !== this.announcedArtifact) {
      this.announcedArtifact = sel.artifact;
      emit(this, 'tessera-artifactopen', {id: idString(sel.artifact.id), detail: {...sel.artifact.detail, maskedCount: sel.artifact.detail.maskedCount.toString(10)}});
    }
    const region = s.get('region');
    // The current view's frame converts between data coordinates and world space.
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
        // An artifact selection draws no shape of its own; the opened artifact's outline shows it.
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
          shape: shapeDetail(region.shape),
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
    // The layer redraws itself on projection changes, not on the properties this element
    // computes, so a change in the opened artifact repaints here.
    const opened = sel.artifact?.id ?? null;
    if (opened !== this.paintedOpened) {
      this.paintedOpened = opened;
      this.paint();
    }
    // Likewise for `highlighting` and `washChannel`.
    const wash = washChannel(s.get('filters'), view, region);
    if (view.highlighting !== this.paintedHighlighting || wash !== this.paintedWashChannel) {
      this.paintedHighlighting = view.highlighting;
      this.paintedWashChannel = wash;
      this.paint();
    }
    super.onStoreChange();
  }

  private probedArtifacts: object | null = null;
  private probedComposition: object | null = null;
  /** The opened artifact the last paint drew, so a change repaints once. */
  private paintedOpened: bigint | null = null;
  /** What the last paint told the layer about the highlight. */
  private paintedHighlighting = false;
  private paintedWashChannel: ReturnType<typeof washChannel> | null = null;
  private regionShape: SelectionShape | null = null;

  /** What `measure` adds on a store change: the composition check and the cluster sample. */
  private measureStore(s: Store): void {
    const view = s.get('view');
    if (view.composition && !checkedCompositions.has(view.composition)) {
      checkedCompositions.add(view.composition);
      assertCompositionMatchesServed(view.composition);
    }
    const artifacts = s.get('artifacts');
    if (artifacts === this.probedArtifacts && view.composition === this.probedComposition) return;
    this.probedArtifacts = artifacts;
    this.probedComposition = view.composition;
    this.probe.cluster = this.clusterProbe(clusterLayerOf(s.get('legend').colourBy), artifacts, s.get('marks').bands);
  }

  /** A sample of carried ordinals, each resolved through the table; see {@link MapProbe.cluster}. */
  private clusterProbe(clusterLayer: string | null, a: ArtifactsProjection, bands: MarksProjection['bands']): MapProbe['cluster'] {
    const layer = clusterLayer ?? a.layers[0] ?? null;
    const rows = clusterLayer ? a.colourServed : a.served;
    const rowOrdinals = clusterLayer ? new Set(rows.map((x) => a.table.ordinalOf(x.layer, x.tesseraId))) : a.servedOrdinals;
    const sample: {ordinal: number; resolvedId: string | null}[] = [];
    let coloured = 0;
    if (layer) {
      for (const band of bands) {
        const m = band.membership[layer];
        if (!m) continue;
        for (let i = 0; i < m.distinct.length && sample.length < 16; i++) {
          const ordinal = m.distinct[i]!;
          const resolved = a.table.resolve(ordinal, rowOrdinals, this.clusterLevel ?? undefined);
          const entry = resolved === 0 ? null : a.table.entry(resolved);
          if (a.table.resolve(ordinal, a.colours, this.clusterLevel ?? undefined) !== 0) coloured += 1;
          sample.push({ordinal, resolvedId: entry ? idString(entry.tesseraId) : null});
        }
        if (sample.length >= 16) break;
      }
    }
    return {layer, layersOn: a.layers, coverage: a.coverage, servedIds: rows.map((x) => idString(x.tesseraId)), sample, coloured};
  }

  /** Release the store this map built, and the `Deck`, now. */
  override dispose(): void {
    super.dispose();
    this.deck?.finalize();
    this.deck = null;
    this.slab.clear();
  }

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
      onDeviceInitialized: (device) => this.slab.attach(device),
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
    const input = viewInputOf(s, this.viewState, width, height);
    if (!input) return;
    s.setView(input);
    emit(this, 'tessera-viewchange', {bbox: input.bbox, zoom: this.viewState.zoom, width, height});
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
          clusterLevel: this.clusterLevel ?? undefined,
          selectedWorldXY: this.selectedWorldXY,
          openedArtifact: s.get('selection').artifact?.id ?? null,
          hoveredArtifact: this.hoveredArtifact,
          region: this.regionWorld,
          regionPolygon: this.regionPolygon,
          drag: this.drag,
          dragPolygon: this.dragPolygon,
          wash: this.wash,
          highlighting: s.get('view').highlighting,
          washChannel: washChannel(s.get('filters'), s.get('view'), s.get('region')),
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

  /**
   * The shapes a hover or click is resolved against ({@link contourShapes}), held until the served
   * set, the fetched shapes, the level or the roster change. Each is the artifact's box until its
   * shape is fetched; where boxes overlap, depth and the mark under the pointer decide.
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
   * The artifact a world point is over ({@link hoverAt}). Hover and click both use it, so a click
   * opens what the pointer highlighted. Contours are not in deck's pick pass.
   */
  private artifactAt(world: [number, number], prefer: bigint | null): bigint | null {
    return hoverAt(this.contours(), world, this.hoveredArtifact, TesseraMap.HOVER_MARGIN_PX / 2 ** this.viewState.zoom, prefer);
  }

  /** What the pointer is over: the tooltip from the mark beneath, and the hovered artifact from {@link artifactAt}. */
  private onHover(info: PickingInfo): void {
    const picked = resolvePick(info as never);
    const layerId = (info.sourceLayer ?? info.layer)?.id ?? '';
    const slot = picked.kind === 'mark' ? /marks-p(\d+)$/.exec(layerId) : null;
    const at = slot ? this.slab.markAt(Number(slot[1]), info.index) : null;
    const artifacts = this.resolvedStore?.get('artifacts') ?? null;
    // The mark's own artifact, preferred where shapes interleave.
    const own = at && artifacts ? artifactOfMark(at.band, at.i, artifacts, this.clusterLevel ?? undefined) : null;
    const world = this.worldAt(info.x, info.y);
    this.hoveredArtifact = world ? this.artifactAt(world, own) : null;
    // The viewport carries no shapes; fetch the hovered one. `needShape` asks once per artifact.
    if (this.hoveredArtifact !== null) this.resolvedStore?.needShape(this.hoveredArtifact);
    if (picked.kind !== 'mark') {
      if (this.hover) this.hover = null;
      return;
    }
    // An absent value is shown as absent; an absent title falls back as a missing one does.
    const carried = (name: string, absent: string | null): string | null => {
      const column = at?.band.scalars[name];
      if (!at || !column) return null;
      if (!hasValue(column, at.i)) return absent;
      const raw = (column.values as ArrayLike<unknown>)[at.i];
      return raw === null || raw === undefined ? null : hoverText(raw, column.arrowType);
    };
    const lines = this.tooltipFields
      .split(/[\s,]+/)
      .filter(Boolean)
      .map((name) => carried(name, 'absent'))
      .filter((v): v is string => v !== null);
    const title = this.titleField ? carried(this.titleField, null) : null;
    this.hover = {x: info.x, y: info.y, title: title ?? `#${idString(picked.id)}`, lines};
    if (this.titleField && title === null) this.describeHovered(picked.id, info.x, info.y);
    emit(this, 'tessera-hover', {id: idString(picked.id), x: info.x, y: info.y});
  }

  /**
   * The hovered point's title field from its record, after the pointer rests for
   * {@link HOVER_DESCRIBE_MS}, since a text column is not in the marks. An answer that lands after
   * the pointer moved on is dropped.
   */
  private describeHovered(id: bigint, x: number, y: number): void {
    const field = this.titleField;
    if (this.describeTimer !== null) clearTimeout(this.describeTimer);
    this.describeTimer = setTimeout(() => {
      this.describeTimer = null;
      const store = this.resolvedStore;
      if (!store) return;
      const arrowType = store.get('meta')?.declaredScalars.find((d) => d.name === field)?.arrowType ?? null;
      void store.describe(id).then((fields) => {
        const value = fields?.[field];
        if (value === null || value === undefined || value === '') return;
        if (!this.hover || this.hover.x !== x || this.hover.y !== y) return;
        this.hover = {...this.hover, title: hoverText(value, arrowType)};
      });
    }, HOVER_DESCRIBE_MS);
  }

  private onClick(info: PickingInfo): void {
    const s = this.resolvedStore;
    const picked: Picked = resolvePick(info as never);
    switch (picked.kind) {
      case 'artifact':
        this.lastPick = null;
        // The card's request does not bring the shape into the projection the map reads.
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
        // Contours are not in deck's pick pass: resolve the click as the hover, else a miss.
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
   * Selection gestures are taken in the capture phase, before deck's input layer sees them. deck
   * (mjolnir/hammer) listens for `pointerdown` on the canvas and for move and up on `window`. If it
   * saw a selection's `pointerdown` but not its `pointerup`, its session would stay pressed and the
   * next pointer movement would pan. Stopping `pointerdown` first means no session opens.
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
      // A vertex per pointer move, thinned to about a pixel.
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
    emit(this, 'tessera-selectchange', {shape: shapeDetail(shape), status: shape ? 'loading' : 'cleared'});
  }

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
   * Centre the camera on a data coordinate at the current zoom. Returns false before `meta`, when
   * there is no frame to convert against.
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
   * Fit a box in data coordinates, the space `tessera-viewchange` reports. The camera keeps the
   * canvas's aspect, so the box shown contains the one asked for, and the next
   * `tessera-viewchange` reports the box shown.
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

  override render() {
    const status = this.resolvedStore?.get('status') ?? null;
    const state: PanelState = stateOf(status);
    // Loading and retrying are the status strip's; the map draws the states that could otherwise
    // read as an empty corpus.
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
          // deck reports picks only while the pointer is over the canvas, so clear the hover here.
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

/** A value as the hover shows it; a timestamp in full, as an ISO date-time. */
function hoverText(value: unknown, arrowType: string | null): string {
  if (arrowType === 'timestamp_us' && (typeof value === 'number' || typeof value === 'bigint')) return timestampText(value);
  return String(value);
}

/** Compositions whose fidelity check has run. */
const checkedCompositions = new WeakSet<object>();

attachContextRoot();
defineOnce('tessera-map', TesseraMap);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-map': TesseraMap;
  }
}
