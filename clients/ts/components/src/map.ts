import {Deck, OrthographicView, type Layer, type PickingInfo} from '@deck.gl/core';
import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {
  MAX_DEPTH,
  WORLD_SIZE,
  dataToWorldXY,
  type ArtifactsProjection,
  type MarksProjection,
  type Store,
  type SelectionShape
} from '@tesseradb/client';
import {assertCompositionMatchesServed, chosenColour, hasValue} from '@tesseradb/client/internal';
import {DEFAULT_DENSITY_CELL_PX, DEFAULT_DENSITY_SCALE, DensityCounter, TesseraLayer, densityCountAt, resolvePick, viewInputOf, type Picked, type ResolutionStop} from '@tesseradb/deck';
import {DENSITY_COLOUR_TITLES, MarkSlab, artifactOfMark, clusterLayerOf, contourShapes, densityStops, drawnCells, encodingOf, encodingSignature, hoverAt, maxCount, type ContourShape} from '@tesseradb/deck/internal';
import type {CategoryPaletteName, Colouring, DensityColours, DensityMode, DensityScale, RampName, RampScale, SizeScale, Sizing} from '@tesseradb/deck';
import type {PaletteName, Quantisation, Rgba} from '@tesseradb/client';
import {TesseraElement, emit, idString, shapeDetail, timestampText, type PickOutcome} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf, type PanelState} from './states.js';
import {icon} from './icons.js';
import {sameFrame} from './view-switch.js';
import {chrome, tokens} from './tokens.js';
import './count.js';
import {colouringOf, setColouring, setSizing, sizingOf, watchChoices} from './colouring.js';


/** How long after disconnection the `Deck` is finalised, unless the element reconnects. */
const FINALIZE_SETTLE_MS = 250;
/** How long the pointer must rest on a mark before its record is asked for. */
const HOVER_DESCRIBE_MS = 140;

/**
 * What the map reports about itself for instruments and tests, one object mutated in place.
 * `timings.frame` and `cluster` are filled only while `measure` is on.
 *
 * @internal
 */
export type MapProbe = {
  paints: number;
  at: number;
  marks: number;
  /** Viewport requests the store has issued, when its instruments report them; else 0. */
  requests: number;
  /** Aggregate requests density has started. */
  densityRequests: number;
  encoding: string;
  view: {depth: number; status: string; stale: boolean; visible: number; matched: number; served: number; provisional: number};
  /** `ms` is from the selection to its counts arriving, on the store's clock. */
  region: {verdict: string; exact: boolean; visible: number | null; matched: number; held: number; status: string; ms: number | null} | null;
  timings: {
    /** The last paint's work per stage, in milliseconds. */
    slabMs: number;
    densityMs: number;
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

/** How many maps have been made, which numbers each one's ask for point columns. */
let mapsMade = 0;

/**
 * The map: the points, the density wash, the artifacts' outlines and labels, hover, pick and the
 * selection, drawn with deck.gl in an orthographic view. The refused, expired and empty states are
 * drawn over the canvas, so none of them reads as an empty corpus. The toolbar switches between
 * pan, box select and lasso select, and fits the whole extent; shift-drag in pan mode draws a box.
 * A settled box or lasso becomes the store's selection, a filter every count narrows to; a tag on
 * its top-left corner gives the count matched inside it and a button that clears it.
 *
 * The map owns the camera and tells the store where it is looking on every move. It starts on the
 * whole extent, fitted as the Fit button fits it, unless the camera was moved before `meta`. Density is asked
 * for once the camera has rested for 200 ms, as counts by cell over the view and a margin round
 * it, and the last counts are drawn at their own positions until the next land. Keys, with the
 * map focused: the arrow keys pan, `+` and `-` zoom, and Escape cancels a shape being drawn, else
 * clears the selected region, else drops the picked point or artifact and its ring. A press hides
 * the hover tooltip until the pointer is released. The canvas is one tab stop, after the toolbar
 * and before the content of the other corners: it carries `role="application"`, the `tabindex`
 * the host set on the map, else 0, and the host's `aria-label` where the host set one, else one
 * naming the keys. A `tabindex` the host sets moves to the canvas, leaving -1 on the map, so the
 * map is never a second stop. `focus()` focuses the canvas.
 *
 * The host element is `display: block`; its height comes from `--tessera-map-height`. A map that
 * is disconnected and not reconnected releases its GPU resources a quarter of a second later.
 *
 * @summary The map canvas.
 * @tagname tessera-map
 * @category Elements
 * @slot top-left - Content in the top-left corner, below the toolbar when it is there.
 * @slot top-right - Content in the top-right corner, below the toolbar when it is there.
 * @slot bottom-left - Content in the bottom-left corner, above the toolbar when it is there.
 * @slot bottom-right - Content in the bottom-right corner, above the toolbar when it is there.
 * @slot tooltip - Replaces the hover tooltip's content.
 * @fires {CustomEvent<TesseraEventDetails['tessera-viewchange']>} tessera-viewchange - The camera
 *   moved.
 * @fires {CustomEvent<TesseraEventDetails['tessera-pick']>} tessera-pick - A point was clicked, and
 *   again with its record once the record arrives.
 * @fires {CustomEvent<TesseraEventDetails['tessera-miss']>} tessera-miss - A click found no point
 *   and no artifact.
 * @fires {CustomEvent<TesseraEventDetails['tessera-hover']>} tessera-hover - The pointer moved onto
 *   or over a point.
 * @fires {CustomEvent<TesseraEventDetails['tessera-artifactopen']>} tessera-artifactopen - An
 *   artifact was opened and its drill-down arrived.
 * @fires {CustomEvent<TesseraEventDetails['tessera-selectchange']>} tessera-selectchange - A
 *   selection was drawn or cleared (`status` `loading` or `cleared`), and again when its counts
 *   arrive.
 * @fires {CustomEvent<TesseraEventDetails['tessera-layerchange']>} tessera-layerchange - The
 *   `layers` attribute changed.
 * @csspart canvas - The deck.gl canvas's container.
 * @csspart overlay - The state drawn over the canvas when refused, expired or empty.
 * @csspart state - The state line inside the overlay, with `data-state`.
 * @csspart retry - The Retry button in the overlay, when the view was refused.
 * @csspart refusal - The words "View refused" inside the overlay, with `data-code` set to the
 *   refusal's code.
 * @csspart controls - The toolbar.
 * @csspart region-tag - The drawn region's tag on its top-left corner: how many items match inside
 *   it, or outside it for its complement, and a button that clears the selection.
 * @csspart tooltip - The hover tooltip.
 * @csspart density-key - The key to density's colours, items per cell from 0 to the largest count
 *   drawn with the count at the scale's midpoint, in the bottom-left corner while density is drawn
 *   in a ramp or without the points.
 * @cssprop --tessera-map-height - The map's height.
 * @cssprop --tessera-map-bg - The canvas's background, behind the points and any basemap.
 * @cssprop --tessera-map-inset-left - Extra space between the top-left corner's content and the
 *   map's left edge, for a panel floated over the map's left side. Defaults to 0.
 */
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
      [part='canvas'] {
        position: absolute;
        inset: 0;
      }
      [part='canvas']:focus-visible {
        outline: 1.5px solid color-mix(in srgb, var(--_tessera-accent) 40%, transparent);
        outline-offset: -1.5px;
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
        gap: calc(var(--_tessera-space) / 2);
        max-width: 46%;
        pointer-events: none;
      }
      .corner > ::slotted(*) {
        pointer-events: auto;
      }
      .top-left {
        top: var(--_tessera-space);
        left: calc(var(--_tessera-space) + var(--tessera-map-inset-left, 0px));
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
        align-self: flex-start;
        display: flex;
        flex-direction: column;
        gap: 2px;
        padding: 3px;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
        pointer-events: auto;
      }
      [part='controls'] button {
        width: var(--_tessera-tool-size, 32px);
        height: var(--_tessera-tool-size, 32px);
        display: grid;
        place-items: center;
        color: color-mix(in srgb, var(--_tessera-ink) 82%, var(--_tessera-surface));
        border-radius: var(--_tessera-radius-control);
      }
      [part='controls'] button:hover {
        background: var(--_tessera-surface-2);
      }
      [part='controls'] button[aria-pressed='true'] {
        background: var(--_tessera-accent);
        color: var(--_tessera-accent-ink);
      }
      /* The drawn region's count and clear button, on its top edge. */
      [part='region-tag'] {
        position: absolute;
        z-index: 3;
        display: flex;
        align-items: center;
        gap: 6px;
        padding: 2px 3px 2px 8px;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius-control);
        box-shadow: var(--_tessera-shadow);
        font-size: 12px;
        color: var(--_tessera-ink-2);
        white-space: nowrap;
        transform: translateY(calc(-100% - 4px));
      }
      [part='region-tag'] tessera-count {
        font-size: inherit;
        color: var(--_tessera-ink);
      }
      [part='region-tag'] tessera-count::part(count) {
        font-weight: 600;
      }
      [part='region-tag'] button {
        width: 20px;
        height: 20px;
        display: grid;
        place-items: center;
        border-radius: 4px;
        color: var(--_tessera-ink-2);
      }
      [part='region-tag'] button:hover {
        background: var(--_tessera-surface-2);
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
        min-height: 40px;
        padding: 6px 8px 6px 14px;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
        font-size: 13px;
        font-weight: 500;
        color: var(--_tessera-ink);
      }
      [part='overlay'] [part='state']:not(:has(button)) {
        padding-right: 14px;
      }
      [part='density-key'] {
        align-self: flex-start;
        display: flex;
        flex-direction: column;
        gap: 3px;
        padding: 7px 9px;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius-control);
        font-size: 11px;
        color: var(--_tessera-ink-2);
        pointer-events: auto;
      }
      [part='density-key'] .ramp {
        display: block;
        width: 120px;
        height: 8px;
        border-radius: 2px;
      }
      [part='density-key'] .ends {
        display: grid;
        grid-template-columns: 1fr auto 1fr;
        gap: 8px;
        font-variant-numeric: tabular-nums;
      }
      [part='density-key'] .ends > :last-child {
        text-align: right;
      }
    `
  ];

  protected override canBuildOwn = true;

  /**
   * What the points are coloured by: a declared column that is rendered, `cluster:<layer>` for a
   * layer that can colour, or `none` for one colour. Unset, the store's choice stands.
   */
  @property({attribute: 'colour-by'}) accessor colourBy = '';
  /**
   * The annotation layers to draw, space- or comma-separated; each is drawn with the layers it
   * depends on. Unset, the store's choice stands.
   */
  @property({
    attribute: 'layers',
    converter: {fromAttribute: (v: string | null) => (v ? v.split(/[\s,]+/).filter(Boolean) : []), toAttribute: (v: string[]) => v.join(' ')}
  })
  accessor layers: string[] | null = null;
  /** How many marks to aim for on screen. `0` leaves the store's budget, which starts at 250000. */
  @property({type: Number}) accessor budget = 0;
  /** The fewest marks a control for `budget` offers. The map sets no budget from it. */
  @property({type: Number, attribute: 'budget-min'}) accessor budgetMin = 1_000;
  /** The most marks a control for `budget` offers. The map sets no budget from it. */
  @property({type: Number, attribute: 'budget-max'}) accessor budgetMax = 2_000_000;
  /**
   * The most clusters a `nested` or `dag` layer is cut to (`Store.setClusterBudget`). `0` at first
   * leaves the store's; `0` after a budget was set clears it, to the finest cut.
   */
  @property({type: Number, attribute: 'cluster-budget'}) accessor clusterBudget = 0;
  /**
   * Columns shown beneath a hovered point's title, space- or comma-separated. The map asks the
   * store for them with `setPointColumns`. Only a rendered column's value is in the marks; another
   * shows nothing.
   */
  @property({attribute: 'tooltip-fields'}) accessor tooltipFields = '';
  /**
   * The field a hovered point is titled by, read from the marks where they carry it (the map asks
   * the store for a rendered one with `setPointColumns`), else from the item's record once the
   * pointer has rested on it for 140 ms. Unset, the title is the point's
   * `tessera_id`.
   */
  @property({attribute: 'title-field'}) accessor titleField = '';
  /** What a drag does: `pan` moves the camera, `box` draws a box selection and `lasso` a freehand one. */
  @property({reflect: true}) accessor mode: 'pan' | 'box' | 'lasso' = 'pan';
  /**
   * The palette clusters are coloured from under colour by cluster: `okabe-ito`, `tableau10`,
   * `tableau20` or `kelly`, set on the store (`Store.setPalette`). Each cluster's colour is the
   * palette's colour at the slot the server gives it. Unset, the store's palette stands, which is
   * Tableau 10 until one is chosen.
   */
  @property() accessor palette: PaletteName | '' = '';
  /** The level to colour and label a nested layer at. Unset, the deepest level served. */
  @property({type: Number, attribute: 'cluster-level'}) accessor clusterLevel: number | null = null;
  /** A deck.gl layer drawn under the points, such as a basemap, in the map's 512-unit world. */
  @property({attribute: false}) accessor basemap: Layer | null = null;
  /**
   * The ground the map draws on, `light` or `dark`, where it differs from the page's, such as a
   * light basemap under a dark page. The labels and the density colours follow it, and the
   * toolbar and panels follow the page. Unset, the host's `color-scheme` decides, else the system
   * preference.
   */
  @property({reflect: true}) accessor ground: 'light' | 'dark' | '' = '';
  /** Hides the points. Density, outlines, labels and the selection are still drawn. */
  @property({type: Boolean, attribute: 'no-points', reflect: true}) accessor noPoints = false;
  /**
   * A fixed point alpha from 0.1 to 1. Unset, points are more transparent the more of them are
   * drawn, so a dense region reads as a density.
   */
  @property({type: Number, attribute: 'point-opacity'}) accessor pointOpacity: number | null = null;
  /**
   * How density is drawn under the points, from exact counts by cell (`POST /v1/aggregate`) and
   * never from the points, which are a sample: `none`, `smooth` (a soft wash), `hex` (hexagons),
   * `grid` (square cells) or `contours` (lines of equal density). It counts the highlighted items
   * under a highlight, the matched items under a filter or selection, else the visible items.
   */
  @property({reflect: true}) accessor density: DensityMode = 'none';
  /**
   * How large density's cells are on screen, in CSS pixels: 32, 24, 16, 12, 8, 6 or 4, and any
   * other size is taken as the nearest of them. The cells asked for are those of the depth nearest
   * that size at the current zoom. Where that depth's cells over the area asked for are more than
   * the server's `selection.maxAggregateCells`, the finest size that fits is drawn instead;
   * {@link TesseraMap.densityStops} says which sizes fit.
   */
  @property({type: Number, attribute: 'density-resolution'}) accessor densityResolution = DEFAULT_DENSITY_CELL_PX;
  /**
   * The colours density is drawn in: `warm-grey`, `viridis`, `cividis`, `magma` or `greys`. Unset,
   * the smooth wash under the points is warm grey and every other density is Viridis. While density
   * is drawn in a ramp, or without the points, a key to the colours sits in the bottom-left corner.
   */
  @property({attribute: 'density-colours'}) accessor densityColours: DensityColours | '' = '';
  /** How strongly density is drawn, from 0.1 to 1. */
  @property({type: Number, attribute: 'density-strength'}) accessor densityStrength = 1;
  /**
   * How a cell's count is placed between no items and the largest count drawn, which picks its
   * colour: `linear`, in proportion to the count, or `log`, in proportion to `log(1 + count)`,
   * which spreads counts that span several orders of magnitude. Log is the default because counts
   * are skewed on most maps, and under linear only the densest few cells stand out from the rest.
   * The largest count is that of the densest cell in the area counted, which is the view and a
   * margin around it; the hexagons and contours take it among the cells they merge to, and a
   * hexagon is coloured by the densest cell inside it. Any other value is taken as `log`.
   */
  @property({attribute: 'density-scale'}) accessor densityScale: DensityScale = DEFAULT_DENSITY_SCALE;
  /**
   * The palette a category column's values are coloured from: `tableau10`, `okabe-ito`, `set2` or
   * `dark2`. Unset, the choice made in the explorer's Colour section stands, Tableau 10 until one
   * is made.
   */
  @property({attribute: 'category-palette'}) accessor categoryPalette: CategoryPaletteName | '' = '';
  /**
   * The ramp a number column's values are coloured on: `viridis`, `cividis`, `magma`, `greys` or
   * `red-blue`. Unset, the choice made in the explorer's Colour section stands, Viridis until one
   * is made.
   */
  @property() accessor ramp: RampName | '' = '';
  /** How numbers are placed on the ramp, `linear` or `log`. Unset, the Colour section's choice stands. */
  @property({attribute: 'ramp-scale'}) accessor rampScale: RampScale | '' = '';
  /** Runs the ramp from its high end to its low end. */
  @property({type: Boolean, attribute: 'ramp-reverse'}) accessor rampReverse = false;
  /**
   * Colours for single category values, per column, per category key, as `#rrggbb`, such as
   * `{field: {'cs.CV': '#f28e2b'}}`. Setting it replaces every value colour chosen before,
   * including those chosen on a field card; a host restores a viewer's saved choices this way,
   * having kept them from `tessera-valuecolour`. Unset, the choices made on the cards stand.
   */
  @property({attribute: false}) accessor valueColours: Colouring['values'] | null = null;
  /**
   * Colours for single clusters, per layer, per `tessera_id` as a decimal string, as `#rrggbb`, such
   * as `{topics: {'4021': '#f28e2b'}}`, set on the store in place of their palette colours
   * (`Store.setArtifactColours`). Setting it replaces every cluster colour chosen before, including
   * those chosen on a field card or in Edit colours; a host restores a viewer's saved choices this
   * way, having kept them from `tessera-clustercolour`. An entry that is not a `tessera_id` and a
   * `#rrggbb` colour is skipped. Unset, the choices made on the cards stand.
   */
  @property({attribute: false}) accessor clusterColours: Record<string, Record<string, string>> | null = null;
  /**
   * Whether the map measures itself for the probe: the frame-gap loop behind `probe.timings.frame`,
   * the colour-by-cluster sample behind `probe.cluster`, and the check that each composition
   * matches what was served. Off, none of the three runs.
   *
   * @internal
   */
  @property({type: Boolean}) accessor measure = false;
  /**
   * A fixed point radius in pixels. Unset, points are sized by how many are drawn and by the zoom.
   * Under `size-by`, the size column sizes the points in its place.
   */
  @property({type: Number}) accessor radius: number | null = null;
  /**
   * What the points are sized by: a declared number column that is rendered, or `none` for one
   * size. Each point's value is placed between `size-min` and `size-max` on `size-scale`, against
   * the range of the values drawn, or under rank a sample of them; a point with no value, NaN or an
   * infinity draws as a ring, at `size-min` or 3 px, whichever is larger. The column arrives with
   * the points, so choosing one sends no request. Unset, the store's choice stands.
   */
  @property({attribute: 'size-by'}) accessor sizeBy = '';
  /**
   * The radius in pixels of the smallest value under `size-by`. A value that is not a finite number
   * above zero is ignored. Unset, the choice made in the explorer stands, 2 until one is made.
   */
  @property({type: Number, attribute: 'size-min'}) accessor sizeMin: number | null = null;
  /**
   * The radius in pixels of the largest value under `size-by`. A value that is not a finite number
   * above zero is ignored. Unset, the choice made in the explorer stands, 9 until one is made.
   */
  @property({type: Number, attribute: 'size-max'}) accessor sizeMax: number | null = null;
  /**
   * How values are placed between the two sizes: `linear`, `log`, or `rank` among a sample of the
   * values drawn. Unset, the choice made in the explorer stands, linear until one is made.
   */
  @property({attribute: 'size-scale'}) accessor sizeScale: SizeScale | '' = '';
  /** Hides the toolbar. */
  @property({type: Boolean, attribute: 'no-controls'}) accessor noControls = false;
  /**
   * Which corner the toolbar sits in: `top-left`, `top-right`, `bottom-left` or `bottom-right`. In
   * a top corner the corner's slotted content is below the toolbar; in a bottom corner, above it.
   */
  @property({attribute: 'controls-corner'}) accessor controlsCorner: 'top-left' | 'top-right' | 'bottom-left' | 'bottom-right' = 'top-left';

  /** @internal */
  @state() accessor hover: {x: number; y: number; title: string; lines: string[]} | null = null;
  /** The artifact under the pointer (its outline, label or a mark it holds), whose outline is drawn. @internal */
  @state() accessor hoveredArtifact: bigint | null = null;
  /** @internal */
  @state() accessor drag: [number, number, number, number] | null = null;
  /** @internal */
  @state() accessor dragPolygon: [number, number][] | null = null;

  /**
   * What the last click found where it found no item: `{kind: 'miss'}`, or a broken pick, a fault
   * in the layer, with its details. `null` after a hit. `<tessera-item-card>`'s `pick` takes it.
   */
  lastPick: PickOutcome = null;
  /**
   * What the last click picked and where: an item or an artifact, by `tesseraId`, and the world
   * position the card for it points at, the picked point's own or, for an artifact, where the click
   * landed. `null` before a pick, after a miss and after a switch of view. `<tessera-explorer>`
   * places its callout from it through {@link TesseraMap.screenOf}.
   */
  pickedAt: {kind: 'item' | 'artifact'; id: bigint; world: [number, number]} | null = null;
  /** The map's probe, one object mutated in place, for instruments and tests. @internal */
  readonly probe: MapProbe = {
    paints: 0,
    at: 0,
    marks: 0,
    requests: 0,
    densityRequests: 0,
    encoding: 'uniform',
    view: {depth: 0, status: 'idle', stale: false, visible: 0, matched: 0, served: 0, provisional: 0},
    region: null,
    timings: {slabMs: 0, densityMs: 0, lutMs: 0, outlinesMs: 0, labelsMs: 0, layersMs: 0, lutWrites: 0, outlines: 0, outlinesDrawn: 0, labels: 0, markRadius: 0, markAlpha: 0, markCount: 0, frame: {mean: 0, p95: 0, n: 0}, decodeMs: []},
    cluster: {layer: null, layersOn: [], coverage: {current: 0, stale: 0}, servedIds: [], sample: [], coloured: 0}
  };

  /** Passed to the layer, so a hover can read a mark's band from it. @internal */
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

  static override get observedAttributes(): string[] {
    return [...super.observedAttributes, 'tabindex', 'aria-label'];
  }

  /** The tab index the canvas takes: the one the host set on the map, else 0. */
  private canvasTab = '0';
  /** Set while the map writes its own `tabindex`, so the write is not read as the host's. */
  private movingTab = false;

  /**
   * A `tabindex` the host sets on the map moves to the canvas, and the map keeps -1 so it is not a
   * second stop; an `aria-label` names the canvas.
   */
  override attributeChangedCallback(name: string, old: string | null, value: string | null): void {
    super.attributeChangedCallback(name, old, value);
    if (name === 'tabindex' && !this.movingTab && value !== null) {
      this.canvasTab = value;
      this.movingTab = true;
      this.setAttribute('tabindex', '-1');
      this.movingTab = false;
      this.requestUpdate();
    }
    if (name === 'aria-label') this.requestUpdate();
  }

  override connectedCallback(): void {
    super.connectedCallback();
    this.askPointColumns(this.resolvedStore);
    if (this.finalizeTimer) {
      clearTimeout(this.finalizeTimer);
      this.finalizeTimer = null;
    }
    if (this.measure) this.startFrameLoop();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.askPointColumns(null);
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
      this.pushSizing(s, changed);
      if (changed.has('sizeBy')) this.pushSizeBy(s);
      if (changed.has('layers') && this.layers) {
        s.setLayers(this.layers);
        emit(this, 'tessera-layerchange', {layers: this.layers});
      }
      if (changed.has('budget') && this.budget > 0) s.setBudget(this.budget);
      if (changed.has('clusterBudget')) {
        if (this.clusterBudget > 0) s.setClusterBudget(this.clusterBudget);
        else if ((changed.get('clusterBudget') ?? 0) > 0) s.setClusterBudget(null);
      }
      if (changed.has('palette') && this.palette !== '') s.setPalette(this.palette);
      if (changed.has('clusterColours')) this.pushClusterColours(s);
      if (changed.has('tooltipFields') || changed.has('titleField')) this.askPointColumns(s);
      this.pushColouring(s, changed);
    }
    if (changed.has('density') || changed.has('densityResolution')) this.pushDensity();
    const repaint = ['mode', 'drag', 'dragPolygon', 'basemap', 'ground', 'radius', 'clusterLevel', 'hoveredArtifact', 'noPoints', 'pointOpacity', 'density', 'densityColours', 'densityStrength', 'densityScale'] as const;
    if (repaint.some((k) => changed.has(k))) this.paint();
  }

  /** The colour properties the host set, written to the choices every element over `store` shares. */
  private pushColouring(store: Store, changed?: PropertyValues<this>): void {
    const touched = (k: 'categoryPalette' | 'ramp' | 'rampScale' | 'rampReverse' | 'valueColours') => !changed || changed.has(k);
    const patch: Partial<Colouring> = {};
    if (touched('categoryPalette') && this.categoryPalette !== '') patch.palette = this.categoryPalette;
    if (touched('ramp') && this.ramp !== '') patch.ramp = this.ramp;
    if (touched('rampScale') && this.rampScale !== '') patch.scale = this.rampScale;
    if (changed ? changed.has('rampReverse') && changed.get('rampReverse') !== undefined : this.rampReverse) patch.reverse = this.rampReverse;
    if (touched('valueColours') && this.valueColours !== null) patch.values = this.valueColours;
    if (Object.keys(patch).length > 0) setColouring(store, patch);
  }

  /** The cluster colours the host set, on `store`; unset leaves the store's as they stand. */
  private pushClusterColours(store: Store): void {
    if (this.clusterColours === null) return;
    const chosen = new Map<string, Map<bigint, Rgba>>();
    for (const [layer, colours] of Object.entries(this.clusterColours)) {
      const own = new Map<bigint, Rgba>();
      for (const [id, hex] of Object.entries(colours ?? {})) {
        const rgba = /^\d+$/.test(id) ? chosenColour(hex) : null;
        if (rgba) own.set(BigInt(id), rgba);
      }
      chosen.set(layer, own);
    }
    store.setArtifactColours(chosen);
  }

  /** The size properties the host set, written to the choices every element over `store` shares. */
  private pushSizing(store: Store, changed?: PropertyValues<this>): void {
    const touched = (k: 'sizeMin' | 'sizeMax' | 'sizeScale') => !changed || changed.has(k);
    const patch: Partial<Sizing> = {};
    if (touched('sizeMin') && this.sizeMin !== null) patch.min = this.sizeMin;
    if (touched('sizeMax') && this.sizeMax !== null) patch.max = this.sizeMax;
    if (touched('sizeScale') && this.sizeScale !== '') patch.scale = this.sizeScale;
    if (Object.keys(patch).length > 0) setSizing(store, patch);
  }

  /** The id the map's ask for the hover's columns goes under, its own among the maps on one store. */
  private readonly pointColumnsId = `map#${++mapsMade}`;
  /** The store the hover's columns were last asked of, and what was asked. */
  private pointColumnsAsked: {store: Store; columns: string} | null = null;

  /**
   * Ask `store` for the columns the hover reads from the marks, `tooltip-fields` and
   * `title-field`, withdrawing the ask from a store the map has left; `null` withdraws it.
   */
  private askPointColumns(store: Store | null): void {
    const columns = [...new Set([...this.tooltipFields.split(/[\s,]+/), this.titleField].filter(Boolean))];
    const asked = this.pointColumnsAsked;
    if (asked?.store === store && asked.columns === columns.join(' ')) return;
    if (asked && asked.store !== store) asked.store.setPointColumns(this.pointColumnsId, []);
    this.pointColumnsAsked = store ? {store, columns: columns.join(' ')} : null;
    store?.setPointColumns(this.pointColumnsId, columns);
  }

  /** Whether the store was last told to sample the size column, for sizing by rank. */
  private sizeRank = false;

  /** The column `size-by` names, sent to the store with whether the scale in force is rank. */
  private pushSizeBy(store: Store): void {
    if (this.sizeBy === '') return;
    this.sizeRank = sizingOf(store).scale === 'rank';
    store.setSizeBy(this.sizeBy === 'none' ? null : this.sizeBy, {rank: this.sizeRank});
  }

  /** Tell the store to start or stop sampling the size column as the scale moves to or from rank. */
  private followSizeRank(store: Store): void {
    const column = store.get('legend').sizeBy;
    const rank = sizingOf(store).scale === 'rank';
    if (column === null || rank === this.sizeRank) return;
    this.sizeRank = rank;
    store.setSizeBy(column, {rank});
  }

  /** The ground: `ground` if set, else the host's `color-scheme`, else the system preference. */
  private scheme(): 'light' | 'dark' {
    if (this.ground === 'light' || this.ground === 'dark') return this.ground;
    if (typeof getComputedStyle === 'undefined') return 'dark';
    const declared = getComputedStyle(this).colorScheme ?? '';
    const dark = /dark/.test(declared);
    const light = /light/.test(declared);
    if (dark && !light) return 'dark';
    if (light && !dark) return 'light';
    return typeof matchMedia !== 'undefined' && matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }

  protected override resetServerData(): void {
    // A hover's title is a record the server answered, and a pick names an item it served.
    this.hover = null;
    this.hoveredArtifact = null;
    this.pickedAt = null;
  }

  /** Stops following the colour and size choices of the store adopted last. */
  private unwatchChoices: (() => void) | null = null;

  /** Keeps the counts density is drawn from, over the store adopted last. */
  private densityCounter: DensityCounter | null = null;

  /** Tell the density counter what to draw: whether, and at which cell size. */
  private pushDensity(): void {
    this.densityCounter?.set({on: this.density !== 'none', cellPx: this.densityResolution});
  }

  /**
   * The resolution stops at the camera as it stands: each cell size, the depth it asks for, and
   * whether that depth's cells over the area asked for fit the server's
   * `selection.maxAggregateCells`. Every stop is enabled before `meta` has arrived.
   */
  densityStops(): ResolutionStop[] {
    return this.densityCounter?.stops() ?? [];
  }

  protected override onStoreAdopted(store: Store | null): void {
    this.densityCounter?.dispose();
    this.densityCounter = store
      ? new DensityCounter(store, () => {
          this.probe.densityRequests = this.densityCounter?.requests ?? 0;
          this.paint();
          // The key's figures follow the counts.
          this.requestUpdate();
        })
      : null;
    this.pushDensity();
    this.unwatchChoices?.();
    this.unwatchChoices = store
      ? watchChoices(store, () => {
          this.followSizeRank(store);
          this.paint();
        })
      : null;
    this.slab.clear();
    this.metaSeen = false;
    // A new store's view starts on its whole extent, as the first did.
    this.cameraSet = false;
    this.selectedWorldXY = null;
    this.pickedAt = null;
    this.regionWorld = null;
    this.regionPolygon = null;
    this.askPointColumns(store);
    if (!store) {
      this.paint();
      return;
    }
    if (this.colourBy !== '') store.setColourBy(this.colourBy === 'none' ? null : this.colourBy);
    if (this.layers) store.setLayers(this.layers);
    if (this.budget > 0) store.setBudget(this.budget);
    if (this.clusterBudget > 0) store.setClusterBudget(this.clusterBudget);
    if (this.palette !== '') store.setPalette(this.palette);
    this.pushClusterColours(store);
    this.pushColouring(store);
    this.pushSizing(store);
    this.pushSizeBy(store);
    this.paint();
  }

  protected override onStoreChange(): void {
    const s = this.resolvedStore;
    if (!s) return;
    if (!this.metaSeen && s.get('meta')) {
      this.metaSeen = true;
      // A camera no one has moved starts on the whole extent, where the map has a size to fit.
      if (!this.cameraSet && this.clientWidth > 0 && this.clientHeight > 0) this.fit();
      else this.pushView();
    }
    const view = s.get('view');
    // Every switch drops the hover: the marks under the cursor are different rows in the new view.
    if (view.id !== this.cameraView) {
      if (this.cameraView !== '') this.pickedAt = null;
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
    p.encoding = clusterLayer ? `cluster|${clusterLayer}` : encodingSignature(encodingOf(s.get('meta'), legend, colouringOf(s)));
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
    // Likewise for `highlighting`.
    if (view.highlighting !== this.paintedHighlighting) {
      this.paintedHighlighting = view.highlighting;
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

  /** Dispose of the store this map built, as on every element, and release its GPU resources now. */
  override dispose(): void {
    this.densityCounter?.dispose();
    this.densityCounter = null;
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
      // The map takes the keys itself, on its canvas, so deck's keyboard is off.
      controller: {keyboard: false},
      pickingRadius: 8,
      layers: [],
      onDeviceInitialized: (device) => this.slab.attach(device),
      // The canvas's container is the map's one tab stop and takes its keys; deck's input handling
      // makes its own canvas focusable once it is set up, so that is undone here.
      onLoad: () => parent.querySelector('canvas')?.setAttribute('tabindex', '-1'),
      onViewStateChange: ({viewState}) => {
        const v = viewState as {target: number[]; zoom: number};
        this.cameraSet = true;
        this.viewState = {...this.viewState, target: [v.target[0]!, v.target[1]!, 0], zoom: v.zoom};
        this.deck?.setProps({viewState: this.viewState});
        // The tooltip names what was under the pointer before the move.
        if (this.hover) this.hover = null;
        this.pushView();
        // The region's tag follows the camera.
        if (this.regionWorld || this.regionPolygon) this.requestUpdate();
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
    this.densityCounter?.look({target: this.viewState.target, zoom: this.viewState.zoom, width, height});
    this.probe.densityRequests = this.densityCounter?.requests ?? 0;
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
          highlighting: s.get('view').highlighting,
          points: !this.noPoints,
          radius: this.radius,
          pointOpacity: this.pointOpacity,
          density: this.density,
          densityCounter: this.densityCounter,
          densityColours: this.densityColours || null,
          densityStrength: this.densityStrength,
          densityScale: drawnDensityScale(this.densityScale),
          colouring: colouringOf(s),
          sizing: sizingOf(s),
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
              densityMs: t.densityMs,
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
  private contoursHeld: {served: object; fetched: object; level: number | null; meta: object | null; opened: bigint | null; shapes: ContourShape[]} | null = null;

  private contours(): ContourShape[] {
    const s = this.resolvedStore;
    const a = s?.get('artifacts') ?? null;
    if (!a) return [];
    const meta = s?.get('meta') ?? null;
    const level = this.clusterLevel ?? null;
    const opened = s?.get('selection').artifact?.id ?? null;
    const held = this.contoursHeld;
    if (held && held.served === a.served && held.fetched === a.shapes && held.level === level && held.meta === meta && held.opened === opened) return held.shapes;
    const shapes = contourShapes(a, {level: level ?? undefined, meta, opened});
    this.contoursHeld = {served: a.served, fetched: a.shapes, level, meta, opened, shapes};
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
  /** Whether a pointer is pressed on the map, during which no tooltip shows. */
  private pressed = false;

  private onHover(info: PickingInfo): void {
    const picked = resolvePick(info as never);
    const layerId = (info.sourceLayer ?? info.layer)?.id ?? '';
    const slot = picked.kind === 'mark' ? /marks-p(\d+)(?:-(?:dull|lit))?$/.exec(layerId) : null;
    const at = slot ? this.slab.markAt(Number(slot[1]), info.index) : null;
    const artifacts = this.resolvedStore?.get('artifacts') ?? null;
    // The mark's own artifact, preferred where shapes interleave.
    const own = at && artifacts ? artifactOfMark(at.band, at.i, artifacts, this.clusterLevel ?? undefined) : null;
    const world = this.worldAt(info.x, info.y);
    this.hoveredArtifact = world ? this.artifactAt(world, own) : null;
    // The viewport carries no shapes; fetch the hovered one. `needShape` asks once per artifact.
    if (this.hoveredArtifact !== null) this.resolvedStore?.needShape(this.hoveredArtifact);
    if (picked.kind !== 'mark' || this.pressed) {
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

  /** The element that takes the map's focus: the canvas, a tab stop of its own. @internal */
  get focusTarget(): HTMLElement | null {
    return this.renderRoot.querySelector<HTMLElement>('[part="canvas"]');
  }

  /** Focus the canvas, which takes the map's keys. */
  override focus(options?: FocusOptions): void {
    const target = this.focusTarget;
    if (target) target.focus(options);
    else super.focus(options);
  }

  /** Forget the last pick: the card it opened and the ring drawn round the picked point. */
  clearPick(): void {
    this.lastPick = null;
    this.pickedAt = null;
    this.pickedId = null;
    if (this.selectedWorldXY === null) return;
    this.selectedWorldXY = null;
    this.paint();
  }

  private onClick(info: PickingInfo): void {
    const s = this.resolvedStore;
    const picked: Picked = resolvePick(info as never);
    switch (picked.kind) {
      case 'artifact': {
        this.lastPick = null;
        const world = this.worldAt(info.x, info.y);
        this.pickedAt = world ? {kind: 'artifact', id: picked.id, world} : null;
        // The card's request does not bring the shape into the projection the map reads.
        s?.needShape(picked.id);
        void s?.openArtifact(picked.id);
        return;
      }
      case 'mark': {
        this.lastPick = null;
        this.pickedId = picked.id;
        this.selectedWorldXY = picked.worldXY;
        const world = picked.worldXY ?? this.worldAt(info.x, info.y);
        this.pickedAt = world ? {kind: 'item', id: picked.id, world} : null;
        emit(this, 'tessera-pick', {id: idString(picked.id)});
        void s?.pick(picked.id);
        this.paint();
        return;
      }
      case 'miss': {
        // Contours are not in deck's pick pass: resolve the click as the hover, else a miss.
        const world = this.worldAt(info.x, info.y);
        const id = world ? this.artifactAt(world, null) : null;
        if (id === null) {
          this.lastPick = {kind: 'miss'};
          this.pickedAt = null;
          emit(this, 'tessera-miss', {});
          return;
        }
        this.lastPick = null;
        this.pickedAt = world ? {kind: 'artifact', id, world} : null;
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
    // A press starts a drag or a click, and the tooltip would stay where it was through either; it
    // comes back on the next hover after the press ends.
    if (this.hover) this.hover = null;
    this.pressed = true;
    addEventListener('pointerup', () => (this.pressed = false), {once: true});
    addEventListener('pointercancel', () => (this.pressed = false), {once: true});
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

  /**
   * Select a shape in data coordinates, as drawing a box or lasso does; `null` clears the
   * selection. Fires `tessera-selectchange`.
   */
  select(shape: SelectionShape | null): void {
    this.regionAskedAt = performance.now();
    if (this.probe.region) this.probe.region = null;
    this.resolvedStore?.select(shape);
    emit(this, 'tessera-selectchange', {shape: shapeDetail(shape), status: shape ? 'loading' : 'cleared'});
  }

  /** Whether the camera has been moved, by the user or the host, since the map was made. */
  private cameraSet = false;

  private setViewState(next: Partial<ViewState>): void {
    this.cameraSet = true;
    this.viewState = {...this.viewState, ...next};
    this.deck?.setProps({viewState: this.viewState});
    this.pushView();
    if (this.regionWorld || this.regionPolygon) this.requestUpdate();
  }

  /**
   * The drawn region's tag at its top-left corner: how many items match inside it (or outside, for
   * its complement) and a button that clears it. A region drawn as an artifact's outline has none.
   */
  private regionTag(): TemplateResult | typeof nothing {
    const region = this.resolvedStore?.get('region');
    if (!region) return nothing;
    let corner: [number, number] | null = null;
    if (this.regionWorld) corner = [this.regionWorld[0], this.regionWorld[1]];
    else if (this.regionPolygon && this.regionPolygon.length > 0) {
      corner = [Math.min(...this.regionPolygon.map((p) => p[0])), Math.min(...this.regionPolygon.map((p) => p[1]))];
    }
    if (!corner) return nothing;
    // The orthographic camera, y down: world units scale by 2^zoom about the target at the centre.
    const {width, height} = this.size;
    const scale = 2 ** this.viewState.zoom;
    const [tx, ty] = this.viewState.target;
    // Kept inside the map where the corner is off its top edge, and no further left than the
    // top-left corner's content starts, right of whatever the host keeps over the map's left side.
    const left = (corner[0] - tx!) * scale + width / 2;
    const top = Math.max(30, (corner[1] - ty!) * scale + height / 2);
    return html`<div part="region-tag" style=${`left:max(calc(var(--_tessera-space) + var(--tessera-map-inset-left, 0px)), ${left}px);top:${top}px`}>
      <tessera-count .masked=${region.matched}></tessera-count><span>${region.shape.outside ? 'outside' : 'inside'}</span>
      <button type="button" aria-label="Clear selection" title="Clear selection" @click=${() => this.select(null)}>${icon('close', 12, 1.4)}</button>
    </div>`;
  }

  /** The ground the map draws on now: `ground` where it is set, else the page's colour scheme. */
  get drawnGround(): 'light' | 'dark' {
    return this.scheme();
  }

  /**
   * A world position in the map's pixels, from its top-left corner, under the camera as it stands.
   * A position off the map falls outside `[0, width] × [0, height]`.
   */
  screenOf(world: readonly [number, number]): [number, number] {
    // The orthographic camera, y down: world units scale by 2^zoom about the target at the centre.
    const {width, height} = this.size;
    const scale = 2 ** this.viewState.zoom;
    const [tx, ty] = this.viewState.target;
    return [(world[0] - tx!) * scale + width / 2, (world[1] - ty!) * scale + height / 2];
  }

  /** The camera's zoom: 0 when the 512-unit world fills 512 px, +1 per doubling. */
  get zoom(): number {
    return this.viewState.zoom;
  }

  /**
   * Fit the whole extent into the map's narrower axis, at 92% of it, so the points at the extent's
   * edges and their labels stay on screen.
   */
  fit(): void {
    const {width, height} = this.size;
    this.setViewState({target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0], zoom: Math.log2((0.92 * Math.min(width, height)) / WORLD_SIZE)});
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

  /** Fit a served artifact's box into the map. Returns false where the store holds no box for it. */
  fitTo(artifactId: bigint): boolean {
    const extent = this.resolvedStore?.extentOf(artifactId);
    if (!extent) return false;
    return this.fitBbox(extent);
  }

  /**
   * Fit a box `[x0, y0, x1, y1]` in data coordinates, the space `tessera-viewchange` reports, into
   * the map. The camera keeps the canvas's aspect, so the box shown contains the one asked for,
   * and the next `tessera-viewchange` reports the box shown. Returns false before `meta`.
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
    // From the canvas's container, or deck's canvas inside it, which a click focuses.
    if (e.target !== e.currentTarget && !(e.target instanceof HTMLCanvasElement)) return;
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
        } else if (this.pickedAt || this.selectedWorldXY) {
          this.clearPick();
          this.resolvedStore?.clearSelection();
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

  /**
   * What density's colours mean, while density is drawn in a ramp or without the points: items per
   * cell at either end of the ramp, 0 and the largest count drawn, and the count at its middle on
   * the scale in force, rounded to whole items and left out where it is below one item or not
   * below the largest. The warm-grey wash under the points is context and has no key.
   */
  private densityKey(): TemplateResult | typeof nothing {
    if (this.density !== 'smooth' && this.density !== 'hex' && this.density !== 'grid') return nothing;
    const colours = drawnDensityColours(this.density, this.densityColours, !this.noPoints);
    if (!this.noPoints && colours === 'warm-grey') return nothing;
    const stops = densityStops(colours, this.scheme()).map(([r, g, b]) => `rgb(${r}, ${g}, ${b})`);
    const counts = this.densityCounter?.counts();
    const max = counts ? maxCount(drawnCells(counts, this.density).cells) : 0;
    const scale = drawnDensityScale(this.densityScale);
    const middle = Math.round(densityCountAt(0.5, max, scale));
    const label = `Density in ${DENSITY_COLOUR_TITLES[colours]}, ${max > 0 ? `from 0 to ${countText(max)} items per cell on a ${scale} scale` : 'from fewer items to more'}`;
    return html`<div part="density-key" role="img" aria-label=${label}>
      <span class="caption">${max > 0 ? 'Items per cell' : 'Density'}</span>
      <span class="ramp" style=${`background:linear-gradient(to right, ${stops.join(', ')})`}></span>
      <span class="ends"
        >${max > 0
          ? html`<span>0</span><span>${middle >= 1 && middle < max ? countText(middle) : ''}</span><span title="The densest cell in view and around it">${countText(max)}</span>`
          : html`<span>Fewer</span><span></span><span>More items</span>`}</span
      >
    </div>`;
  }

  override render(): TemplateResult | typeof nothing {
    const status = this.resolvedStore?.get('status') ?? null;
    const state: PanelState = stateOf(status);
    // Loading and retrying are the status strip's; the map draws the states that could otherwise
    // read as an empty corpus.
    const refresh = () => this.resolvedStore?.refresh();
    const overlay = state === 'refused' || state === 'expired' || state === 'empty' ? html`<div part="overlay">${renderState(state, status, {onRefresh: refresh, onRetry: refresh})}</div>` : nothing;
    const controls = this.noControls
      ? nothing
      : html`<div part="controls" role="toolbar" aria-label="Map tools">
          <button type="button" aria-label="Pan" aria-pressed=${this.mode === 'pan'} title="Pan (shift-drag selects)" @click=${() => (this.mode = 'pan')}>${icon('pan', 16, 1.2)}</button>
          <button type="button" aria-label="Box select" aria-pressed=${this.mode === 'box'} title="Box select" @click=${() => (this.mode = 'box')}>${icon('box', 16, 1.2)}</button>
          <button type="button" aria-label="Lasso select" aria-pressed=${this.mode === 'lasso'} title="Lasso select" @click=${() => (this.mode = 'lasso')}>${icon('lasso', 16, 1.2)}</button>
          <button type="button" aria-label="Fit to extent" title="Fit to extent" @click=${() => this.fit()}>${icon('fit', 16, 1.2)}</button>
        </div>`;
    const slots = {
      'top-left': html`<slot name="top-left"></slot>`,
      'top-right': html`<slot name="top-right"></slot>`,
      'bottom-left': html`<slot name="bottom-left"></slot>`,
      'bottom-right': html`<slot name="bottom-right"></slot>`
    };
    const corner = (at: keyof typeof slots) => {
      const top = at.startsWith('top');
      const own = this.controlsCorner === at ? controls : nothing;
      return html`<div class=${`corner ${at}`}>
        ${top ? own : nothing}${at === 'bottom-left' ? this.densityKey() : nothing}${slots[at]}${top ? nothing : own}
      </div>`;
    };
    const corners = ['top-left', 'top-right', 'bottom-left', 'bottom-right'] as const;
    // The corner holding the toolbar comes before the canvas in the page, and the others after it,
    // so Tab reaches the tools, then the map, then what the host put in the other corners.
    return html`${corner(this.controlsCorner)}<div
        part="canvas"
        tabindex=${this.canvasTab}
        role="application"
        aria-label=${this.getAttribute('aria-label') ?? 'map: arrow keys pan, + and - zoom'}
        @keydown=${this.onKey}
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
      ${this.regionTag()}
      ${corners.filter((c) => c !== this.controlsCorner).map(corner)}
      ${this.hover
        ? html`<div part="tooltip" style=${`left:${this.hover.x}px;top:${this.hover.y}px`}>
            <slot name="tooltip"><div class="t">${this.hover.title}</div>${this.hover.lines.length > 0 ? html`<div class="s">${this.hover.lines.join(' · ')}</div>` : nothing}</slot>
          </div>`
        : nothing}`;
  }
}

/** A count as the density key shows it. */
function countText(n: number): string {
  return n.toLocaleString('en-GB', {maximumFractionDigits: 0});
}

/** The density scale drawn: `linear` where set, else `log`. */
export function drawnDensityScale(scale: string): DensityScale {
  return scale === 'linear' ? 'linear' : 'log';
}

/** The density colours drawn: those chosen, else warm grey for a wash under the points, else Viridis. */
export function drawnDensityColours(density: DensityMode, colours: DensityColours | '', points: boolean): DensityColours {
  return colours || (density === 'smooth' && points ? 'warm-grey' : 'viridis');
}

/** A value as the hover shows it; a timestamp as a date. */
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
