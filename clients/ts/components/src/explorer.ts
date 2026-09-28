import {ContextProvider} from '@lit/context';
import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import type {Store} from '@tesseradb/client';
import type {CategoryPaletteName, Colouring, DensityColours, DensityMode, RampName, RampScale} from '@tesseradb/deck';
import {activeCount, browsableLayers, emptyDraft} from '@tesseradb/client';
import {artifactBudgetFor, hasOneLayout, levelForBudget} from '@tesseradb/client/internal';
import {DENSITY_COLOUR_TITLES, clusterLayerOf} from '@tesseradb/deck/internal';
import './hierarchy.js';
import {TesseraElement, emit} from './base.js';
import {densityGradient, displayStyles, radioKeys, type DisplaySettings} from './display.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon, type IconName} from './icons.js';
import {exportparts, forwarded} from './parts.js';
import {sameFrame} from './view-switch.js';
import {drawnDensityColours, type TesseraMap} from './map.js';
import {chrome, tokens} from './tokens.js';
import './map.js';
import './status.js';
import './filter-panel.js';
import './item-card.js';
import './selection.js';
import './layer-picker.js';
import './view-picker.js';
import './key-picker.js';
import './artifact-list.js';
import './artifact-card.js';
import './legend.js';

/** Every part of every element the explorer renders, forwarded. */
const FORWARD = {
  map: exportparts('map'),
  status: exportparts('status'),
  'view-picker': exportparts('view-picker'),
  'key-picker': exportparts('key-picker'),
  legend: exportparts('legend'),
  'layer-picker': exportparts('layer-picker'),
  'filter-panel': exportparts('filter-panel', forwarded('filter')),
  hierarchy: exportparts('hierarchy'),
  'artifact-list': exportparts('artifact-list'),
  selection: exportparts('selection'),
  'item-card': exportparts('item-card'),
  'artifact-card': exportparts('artifact-card')
};

const ALL_PANELS = ['toolbar', 'legend', 'filters', 'hierarchy', 'artifacts', 'selection', 'detail'] as const;
/** The container widths, in pixels, between which the explorer takes its compact form. */
const COMPACT_BETWEEN = [720, 1000] as const;
type Panel = (typeof ALL_PANELS)[number];
type Sheet = 'filters' | 'layers' | 'artifacts' | 'detail';
/** The Density choices in the Layers popover, in order. */
const DENSITY_MODES: readonly {mode: DensityMode; label: string; icon: IconName}[] = [
  {mode: 'none', label: 'None', icon: 'density-none'},
  {mode: 'smooth', label: 'Smooth', icon: 'density-smooth'},
  {mode: 'hex', label: 'Hex', icon: 'density-hex'},
  {mode: 'grid', label: 'Grid', icon: 'density-grid'},
  {mode: 'contours', label: 'Lines', icon: 'density-lines'}
];
/** The narrow layout's tabs, each opening a sheet, drawn where its panel is. */
const TABS: readonly {sheet: Sheet; icon: IconName; label: string; panel: Panel}[] = [
  {sheet: 'filters', icon: 'filter', label: 'Filters', panel: 'filters'},
  {sheet: 'layers', icon: 'layers', label: 'Layers', panel: 'legend'},
  {sheet: 'artifacts', icon: 'list', label: 'In view', panel: 'artifacts'},
  {sheet: 'detail', icon: 'info', label: 'Item', panel: 'detail'}
];

/**
 * The map with its status strip, toolbar, layers, colour legend, filters, hierarchy, artifact
 * list, selection and detail card, laid out together. It builds its own store from `viewer-url`
 * and `token` or an `authorise` property, or takes a `store` property, and provides it by context
 * to everything inside it, including elements a host puts in its slots.
 *
 * `layout="docked"` puts a sidebar left of the map: the view choice, then Colour (the legend), then
 * Filters (the applied clauses as chips, with Clear all, and every filter control), the selection
 * while a region is selected, and Hierarchy and In view as sections that start collapsed.
 * `layout="overlay"` folds the same content into one card over the map's top-left: the view choice
 * and a Filters button with the number of clauses applied, the chips, then Colour; the Filters
 * button opens the controls inside the card. In both, the map's tools sit at the top-left of the
 * map (beside the card in the overlay layout) with a Layers button beneath them, the detail card
 * sits in the map's top-right corner and the status strip in its bottom-right. The Layers button
 * opens a popover with a Display section (whether the points are drawn, their size and opacity,
 * and how density is drawn, in which colours and how strongly) over the layer picker. The display
 * settings are the explorer's properties of the same names, passed to its map.
 *
 * In a container narrower than 1000 px, either layout folds into the card, 300 px wide, whose
 * legend names four values and offers the rest under "N more"; the tools move to the bottom-left
 * and the strip shortens its figures. In a container 720 px wide or narrower, the status strip runs
 * full width above a tab bar (Filters, Layers, In view, Item), and each tab opens its panel as a
 * sheet. The Hierarchy section appears only where the bundle has a hierarchical layer, and the
 * detail card only once something is selected.
 *
 * Every event its elements fire bubbles out of it, since each is composed.
 *
 * @summary The map and every panel, in a default layout.
 * @tagname tessera-explorer
 * @category Elements
 * @slot toolbar - Replaces the view picker and the key picker.
 * @slot colour - Replaces the legend.
 * @slot layers - Replaces the layer picker, in the Layers popover.
 * @slot filters - Replaces the filter panel.
 * @slot hierarchy - Replaces the hierarchy panel.
 * @slot artifacts - Replaces the In view list.
 * @slot selection - Replaces the selection panel, shown while a region is selected.
 * @slot detail - Replaces the item and artifact cards.
 * @slot status - Replaces the status strip drawn on the map.
 * @slot tooltip - Replaces the map's hover tooltip.
 * @slot top-right - Content in the map's top-right corner, above the detail card.
 * @fires {CustomEvent<TesseraEventDetails['tessera-viewchange']>} tessera-viewchange - The map's camera moved.
 * @fires {CustomEvent<TesseraEventDetails['tessera-pick']>} tessera-pick - A point was clicked, and again with its record.
 * @fires {CustomEvent<TesseraEventDetails['tessera-hover']>} tessera-hover - The pointer moved over a point.
 * @fires {CustomEvent<TesseraEventDetails['tessera-artifactopen']>} tessera-artifactopen - An artifact was opened and its drill-down arrived.
 * @fires {CustomEvent<TesseraEventDetails['tessera-selectchange']>} tessera-selectchange - A selection was drawn, changed or cleared, or its counts arrived.
 * @fires {CustomEvent<TesseraEventDetails['tessera-layerchange']>} tessera-layerchange - The layers chosen changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-colourchange']>} tessera-colourchange - The Colour by choice changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-levelchange']>} tessera-levelchange - The Level choice changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-displaychange']>} tessera-displaychange - A setting in the Display section changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-valuecolour']>} tessera-valuecolour - A colour was chosen or reset for one value in the legend.
 * @fires {CustomEvent<TesseraEventDetails['tessera-palettechange']>} tessera-palettechange - The palette or ramp was chosen in the legend.
 * @fires {CustomEvent<TesseraEventDetails['tessera-statechange']>} tessera-statechange - The status strip's panel state changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-expired']>} tessera-expired - The session expired.
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A filter control or chip changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-clausechange']>} tessera-clausechange - A `member_of` clause was put on or taken off.
 * @fires {CustomEvent<TesseraEventDetails['tessera-artifactselect']>} tessera-artifactselect - A row of the In view list was pressed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-artifactfit']>} tessera-artifactfit - Fit was pressed on the artifact card or in the hierarchy; the explorer fits its map to the artifact.
 * @fires {CustomEvent<TesseraEventDetails['tessera-open']>} tessera-open - Open was pressed on the item card.
 * @fires {CustomEvent<TesseraEventDetails['tessera-close']>} tessera-close - A card's close button was pressed; the explorer drops the selection.
 * @fires {CustomEvent<TesseraEventDetails['tessera-viewswitch']>} tessera-viewswitch - The view changed through the view or key picker.
 * @fires {CustomEvent<TesseraEventDetails['tessera-viewfollow']>} tessera-viewfollow - A view chip on the item card was pressed; the explorer switches to that view and centres on the item.
 * @csspart frame - The explorer's grid.
 * @csspart sidebar - The sidebar, in the docked layout.
 * @csspart panel - The card over the map, in the overlay layout and in a container narrower than
 *   1000 px.
 * @csspart filters-toggle - The card's Filters button, with `aria-expanded` while the controls show
 *   and `data-count` set to the number of clauses applied.
 * @csspart layers-toggle - The Layers button under the map's tools, with `data-on` while any layer
 *   is drawn.
 * @csspart layers-popover - The popover holding the Display section and the layer picker, while it
 *   is open.
 * @csspart display - The Display section at the top of the Layers popover.
 * @csspart points-toggle - The Points switch, with `aria-checked`.
 * @csspart point-size - The Size slider: the points' radius in pixels.
 * @csspart point-opacity - The Opacity slider.
 * @csspart density-mode - The Density choice: None, Smooth, Hex, Grid and Lines, each with
 *   `data-mode` and `aria-checked`.
 * @csspart density-colours - The button that opens the list of density colours, while density is
 *   drawn in colours.
 * @csspart density-strength - The Strength slider, while density is drawn.
 * @csspart detail - The detail card in the map's top-right corner.
 * @csspart sheet - The open sheet, in the narrow layout.
 * @csspart strip-row - The full-width status strip, in the narrow layout.
 * @csspart tabs - The tab bar, in the narrow layout.
 * @csspart map-<part> - A part of the inner `<tessera-map>`, such as `map-controls`.
 * @csspart status-<part> - A part of an inner `<tessera-status>`.
 * @csspart view-picker-<part> - A part of the inner `<tessera-view-picker>`.
 * @csspart key-picker-<part> - A part of the inner `<tessera-key-picker>`.
 * @csspart legend-<part> - A part of the inner `<tessera-legend>`.
 * @csspart layer-picker-<part> - A part of the inner `<tessera-layer-picker>`.
 * @csspart filter-panel-<part> - A part of the inner `<tessera-filter-panel>`.
 * @csspart filter-<part> - A part of a `<tessera-filter>` inside the filter panel.
 * @csspart hierarchy-<part> - A part of the inner `<tessera-hierarchy>`.
 * @csspart artifact-list-<part> - A part of the inner `<tessera-artifact-list>`.
 * @csspart selection-<part> - A part of the inner `<tessera-selection>`.
 * @csspart item-card-<part> - A part of the inner `<tessera-item-card>`, such as `item-card-title`.
 * @csspart artifact-card-<part> - A part of the inner `<tessera-artifact-card>`.
 * @cssprop --tessera-explorer-height - The explorer's height.
 * @cssprop --tessera-sidebar-width - The width of the docked sidebar and of the card over the map.
 */
export class TesseraExplorer extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    displayStyles,
    css`
      :host {
        display: block;
        container-type: inline-size;
        container-name: explorer;
        background: var(--_tessera-map-bg);
        height: var(--tessera-explorer-height, 100%);
        min-height: 320px;
        --_panel-width: var(--tessera-sidebar-width, 340px);
      }
      [part='frame'] {
        position: relative;
        display: grid;
        grid-template-columns: minmax(0, 1fr);
        height: 100%;
        min-height: inherit;
        overflow: hidden;
      }
      [part='frame'].docked {
        grid-template-columns: var(--_panel-width) minmax(0, 1fr);
      }
      [part='frame'].compact {
        --_panel-width: min(var(--tessera-sidebar-width, 340px), 300px);
      }
      .stage {
        position: relative;
        min-width: 0;
        min-height: 0;
        container-type: size;
        container-name: stage;
      }
      tessera-map,
      ::slotted(tessera-map) {
        height: 100%;
        min-height: 320px;
      }
      .floating:not(.compact) tessera-map {
        --tessera-map-inset-left: calc(var(--_panel-width) + var(--_tessera-space));
      }
      [part='sidebar'] {
        display: flex;
        flex-direction: column;
        overflow-y: auto;
        border-right: 1px solid var(--_tessera-line);
        background: var(--_tessera-surface);
      }
      /* The card over the map: the sidebar folded up. */
      [part='panel'] {
        position: absolute;
        z-index: 1;
        top: var(--_tessera-space);
        left: var(--_tessera-space);
        width: var(--_panel-width);
        max-height: calc(100% - 2 * var(--_tessera-space));
        display: flex;
        flex-direction: column;
        overflow-y: auto;
      }
      /* Room beneath for the tools and the Layers button in the bottom-left corner. */
      .compact [part='panel'] {
        max-height: calc(100% - 2 * var(--_tessera-space) - 200px);
      }
      .card {
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
      }
      /* Each section keeps its height, so a long column scrolls rather than squeezing its last section. */
      [part='sidebar'] > *,
      [part='panel'] > * {
        flex: none;
      }
      [part='sidebar'] > *:last-child,
      [part='panel'] > *:last-child {
        border-bottom: 0;
      }
      .head[hidden],
      .pickers[hidden] {
        display: none;
      }
      .head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 10px;
        padding: 16px;
        border-bottom: 1px solid var(--_tessera-line-2);
      }
      [part='panel'] .head {
        padding: 12px 14px;
      }
      .compact [part='panel'] .head {
        padding: 4px 4px 4px 12px;
      }
      .head .pickers {
        display: flex;
        flex-direction: column;
        gap: 8px;
        min-width: 0;
      }
      .head .pickers:empty {
        display: none;
      }
      [part='filters-toggle'] {
        margin-left: auto;
        padding: 0 9px 0 8px;
      }
      .compact [part='filters-toggle'] {
        border: 0;
      }
      .badge {
        min-width: 16px;
        padding: 0 4px;
        border-radius: 8px;
        background: var(--_tessera-accent);
        color: var(--_tessera-accent-ink);
        font-size: 11px;
        line-height: 16px;
        text-align: center;
      }
      [part='panel'] tessera-filter-panel[chips-only]::part(filter-panel-title) {
        margin-bottom: 8px;
      }
      /* The Layers button under the tools, and its popover. */
      .layers {
        position: relative;
        align-self: flex-start;
        pointer-events: auto;
      }
      .layers .group {
        padding: 3px;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
      }
      [part='layers-toggle'] {
        position: relative;
        width: 32px;
        height: 32px;
        display: grid;
        place-items: center;
        border-radius: var(--_tessera-radius-control);
        color: color-mix(in srgb, var(--_tessera-ink) 82%, var(--_tessera-surface));
      }
      [part='layers-toggle']:hover,
      [part='layers-toggle'][aria-expanded='true'] {
        background: var(--_tessera-surface-2);
      }
      [part='layers-toggle'] .on {
        position: absolute;
        top: 3px;
        right: 3px;
        width: 6px;
        height: 6px;
        border-radius: 50%;
        background: var(--_tessera-accent);
      }
      [part='layers-popover'] {
        position: absolute;
        top: 0;
        left: calc(100% + 8px);
        width: 316px;
        max-height: min(560px, calc(100cqh - 2 * var(--_tessera-space)));
        overflow-y: auto;
        box-shadow: 0 6px 24px rgba(0, 0, 0, 0.1);
      }
      [part='layers-popover'] tessera-layer-picker {
        display: block;
        border-top: 1px solid var(--_tessera-line-2);
      }
      .compact [part='layers-popover'] {
        top: auto;
        bottom: 0;
      }
      [part='detail'] {
        width: 320px;
        max-width: 100%;
        max-height: calc(100cqh - 2 * var(--_tessera-space) - 48px);
        overflow-y: auto;
      }
      .compact [part='detail'] {
        width: 272px;
      }
      .right {
        display: flex;
        flex-direction: column;
        align-items: flex-end;
        gap: calc(var(--_tessera-space) / 2);
        pointer-events: none;
      }
      .right > * {
        pointer-events: auto;
      }
      /* Collapsed sections. */
      details {
        border-bottom: 1px solid var(--_tessera-line-2);
      }
      details > summary {
        list-style: none;
        cursor: pointer;
        padding: 12px 16px;
        display: flex;
        align-items: center;
        justify-content: space-between;
        font-size: 11px;
        font-weight: 600;
        letter-spacing: 0.02em;
        text-transform: uppercase;
        color: var(--_tessera-ink-2);
      }
      details > summary::-webkit-details-marker {
        display: none;
      }
      details > summary .t {
        display: flex;
        align-items: center;
        gap: 6px;
      }
      details > summary .summary {
        text-transform: none;
        letter-spacing: 0;
        font-size: 12px;
        font-weight: 400;
        color: var(--_tessera-ink-3);
      }
      details[open] > summary {
        padding-bottom: 0;
      }
      details > summary .open-chev {
        display: none;
      }
      details[open] > summary .open-chev {
        display: inline-flex;
      }
      details[open] > summary .closed-chev {
        display: none;
      }
      details > .body {
        box-sizing: border-box;
        min-width: 0;
      }
      details > .body tessera-hierarchy,
      details > .body tessera-artifact-list {
        display: block;
        max-width: 100%;
      }
      details > .body ::slotted(*),
      details > .body tessera-hierarchy::part(title),
      details > .body tessera-artifact-list::part(title) {
        border-bottom: 0;
      }
      /* The default panels' own headings repeat the summary above them. */
      details > .body tessera-hierarchy::part(title),
      details > .body tessera-artifact-list::part(title) {
        display: none;
      }
      /* The narrow container: the strip full width, a tab bar, sheets. */
      [part='tabs'],
      [part='strip-row'] {
        display: none;
      }
      @container explorer (max-width: 720px) {
        [part='frame'],
        [part='frame'].docked {
          grid-template-columns: minmax(0, 1fr);
          grid-template-rows: minmax(0, 1fr) auto;
        }
        [part='sidebar'],
        [part='panel'],
        .layers,
        .right,
        .in-map-strip {
          display: none;
        }
        [part='tabs'] {
          display: flex;
          height: 56px;
          border-top: 1px solid var(--_tessera-line);
          background: var(--_tessera-surface);
        }
        [part='tabs'] button {
          flex: 1;
          display: flex;
          flex-direction: column;
          align-items: center;
          justify-content: center;
          gap: 3px;
          color: var(--_tessera-ink-2);
          font-size: 11px;
          font-weight: 500;
        }
        [part='tabs'] button[aria-selected='true'] {
          color: var(--_tessera-ink);
          font-weight: 600;
        }
        [part='sheet'] {
          position: absolute;
          left: 0;
          right: 0;
          bottom: 56px;
          max-height: 70%;
          overflow-y: auto;
          background: var(--_tessera-surface);
          border-top: 1px solid var(--_tessera-line);
          border-radius: 12px 12px 0 0;
          box-shadow: var(--_tessera-shadow);
          z-index: 6;
        }
        /* The sheet takes focus as it opens so a keyboard lands in it; that is not a focus to show. */
        [part='sheet']:focus-visible {
          outline: none;
        }
        .sheet-footer {
          position: sticky;
          bottom: 0;
          display: flex;
          gap: 10px;
          padding: 10px 16px;
          border-top: 1px solid var(--_tessera-line-2);
          background: var(--_tessera-surface);
        }
        .sheet-footer .btn {
          height: 44px;
          flex: 1;
          font-size: 13px;
        }
        .sheet-footer .btn.primary {
          flex: 2;
        }
        [part='sheet']::before {
          content: '';
          display: block;
          width: 36px;
          height: 4px;
          border-radius: 2px;
          background: var(--_tessera-line);
          margin: 8px auto 0;
        }
        [part='strip-row'] {
          display: block;
        }
        [part='strip-row'] tessera-status {
          display: block;
        }
        [part='strip-row'] tessera-status::part(strip) {
          display: flex;
          width: 100%;
          box-shadow: none;
          border-radius: 0;
          border-width: 1px 0 0;
          height: 44px;
          overflow-x: auto;
        }
      }
    `
  ];

  protected override canBuildOwn = true;
  /**
   * `docked`, the map beside a sidebar, or `overlay`, the map full-bleed under a card holding the
   * same panels.
   */
  @property({reflect: true}) accessor layout: 'docked' | 'overlay' = 'docked';
  /**
   * Which regions appear, space- or comma-separated, from `toolbar` (the view choice and the
   * legend), `legend` (the Layers button and its layer picker), `filters`, `hierarchy`,
   * `artifacts`, `selection` and `detail`. Defaults to all seven.
   */
  @property() accessor panels: string = ALL_PANELS.join(' ');
  /** Passed to the map's `colour-by`. */
  @property({attribute: 'colour-by'}) accessor colourBy = '';
  /** Passed to the map's `layers`. */
  @property() accessor layers = '';
  /** Passed to the map's `tooltip-fields`. */
  @property({attribute: 'tooltip-fields'}) accessor tooltipFields = '';
  /** The field that titles a point, in the map's tooltip and the item card's headline. Unset, the title is the `tessera_id`. */
  @property({attribute: 'title-field'}) accessor titleField = '';
  /** Passed to the map's `budget`. */
  @property({type: Number}) accessor budget = 0;
  /** Passed to the map's `no-points`; the Points switch in the Layers popover changes it. */
  @property({type: Boolean, attribute: 'no-points'}) accessor noPoints = false;
  /** Passed to the map's `radius`; the Size slider in the Layers popover changes it. */
  @property({type: Number}) accessor radius: number | null = null;
  /** Passed to the map's `point-opacity`; the Opacity slider in the Layers popover changes it. */
  @property({type: Number, attribute: 'point-opacity'}) accessor pointOpacity: number | null = null;
  /** Passed to the map's `density`; the Density choice in the Layers popover changes it. */
  @property() accessor density: DensityMode = 'none';
  /** Passed to the map's `density-colours`; the Colours choice in the Layers popover changes it. */
  @property({attribute: 'density-colours'}) accessor densityColours: DensityColours | '' = '';
  /** Passed to the map's `density-strength`; the Strength slider in the Layers popover changes it. */
  @property({type: Number, attribute: 'density-strength'}) accessor densityStrength = 1;
  /** Passed to the map's `category-palette`. */
  @property({attribute: 'category-palette'}) accessor categoryPalette: CategoryPaletteName | '' = '';
  /** Passed to the map's `ramp`. */
  @property() accessor ramp: RampName | '' = '';
  /** Passed to the map's `ramp-scale`. */
  @property({attribute: 'ramp-scale'}) accessor rampScale: RampScale | '' = '';
  /** Passed to the map's `ramp-reverse`. */
  @property({type: Boolean, attribute: 'ramp-reverse'}) accessor rampReverse = false;
  /** Passed to the map's `valueColours`. */
  @property({attribute: false}) accessor valueColours: Colouring['values'] | null = null;
  /** Passed to the legend's `hide-palettes`: the palette and ramp choices are left out of Colour by. */
  @property({type: Boolean, attribute: 'hide-palettes'}) accessor hidePalettes = false;
  /** Whether the list of density colours in the Layers popover is open. @internal */
  @state() accessor densityColoursOpen = false;
  /** @internal */
  @state() accessor sheet: Sheet | null = null;
  /** The narrow layout's tab focused last, which keeps the tab list's one place in the tab order. */
  @state() private accessor tabFocus: Sheet | null = null;
  /** The level chosen through the legend's select; the map colours and labels at it. @internal */
  @state() accessor level: number | null = null;
  /** Whether the card's Filters button has opened the controls. @internal */
  @state() accessor filtersOpen = false;
  /** Whether the Layers popover is open. @internal */
  @state() accessor layersOpen = false;
  /** Whether the container is narrower than 1000 px and wider than the narrow layout. @internal */
  @state() accessor compact = false;
  /** Whether the host put anything in the toolbar slot. */
  @state() private accessor toolbarFilled = false;

  private onToolbarSlot = (e: Event): void => {
    this.toolbarFilled = (e.target as HTMLSlotElement).assignedElements().length > 0;
  };

  private provider = new ContextProvider(this, {context: storeContext, initialValue: null});
  /** Which selection changed last, which the detail region shows. */
  private lastDetail: 'item' | 'artifact' = 'item';
  private seenItem: object | null = null;
  private seenArtifact: object | null = null;
  private resize: ResizeObserver | null = null;

  protected override onStoreAdopted(store: Store | null): void {
    this.provider.setValue(store);
  }

  protected override onStoreChange(): void {
    const sel = this.resolvedStore?.get('selection');
    if (sel) {
      if (sel.item && sel.item !== this.seenItem) this.lastDetail = 'item';
      if (sel.artifact && sel.artifact !== this.seenArtifact) this.lastDetail = 'artifact';
      if (sel.artifactRefusal && !sel.artifact) this.lastDetail = 'artifact';
      this.seenItem = sel.item;
      this.seenArtifact = sel.artifact;
    }
    super.onStoreChange();
  }

  override connectedCallback(): void {
    super.connectedCallback();
    if (typeof ResizeObserver === 'undefined') return;
    this.resize ??= new ResizeObserver((entries) => {
      const width = entries.at(-1)?.contentRect.width ?? 0;
      this.compact = width > COMPACT_BETWEEN[0] && width < COMPACT_BETWEEN[1];
    });
    this.resize.observe(this);
  }

  override disconnectedCallback(): void {
    // A follow in flight holds its own subscription, which the base class does not drop.
    this.following?.();
    this.resize?.disconnect();
    this.resize = null;
    this.closeLayers();
    super.disconnectedCallback();
  }

  /** Dispose of the store the explorer built and of its map's GPU resources. */
  override dispose(): void {
    this.following?.();
    this.map?.dispose();
    super.dispose();
  }

  /** The `<tessera-map>` the explorer renders, for a host that calls `fit`, `fitTo` or `select`. */
  get map(): TesseraMap | null {
    return this.renderRoot?.querySelector<TesseraMap>('tessera-map') ?? null;
  }

  private has(panel: Panel): boolean {
    return this.panels.split(/[\s,]+/).includes(panel);
  }

  /** A press anywhere outside the Layers button and its popover closes the popover. */
  private onOutside = (e: PointerEvent): void => {
    const group = this.renderRoot.querySelector('.layers');
    if (group && !e.composedPath().includes(group)) this.closeLayers();
  };

  private openLayers(): void {
    this.layersOpen = true;
    document.addEventListener('pointerdown', this.onOutside, true);
  }

  private closeLayers(): void {
    this.layersOpen = false;
    document.removeEventListener('pointerdown', this.onOutside, true);
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const region = s?.get('region') ?? null;
    const selection = s?.get('selection');
    const meta = s?.get('meta') ?? null;
    const filterState = s?.get('filters');
    const applied = filterState ? activeCount(filterState.draft) + filterState.members.length : 0;
    const artifacts = s?.get('artifacts');
    const inView = artifacts && artifacts.status === 'shown' ? (artifacts.lineage.linked ? artifacts.lineage.roots.length : artifacts.served.length) : 0;
    const layersOn = artifacts?.layers.length ?? 0;
    const compact = this.compact;
    // Short of room, the docked layout folds into the card as the overlay does.
    const floating = this.layout === 'overlay' || compact;
    // The level drawn: the one chosen through the legend, else, for a levelled layer served
    // whole, the level the view's budget would cut at, else the deepest served.
    const autoLevel = this.autoLevel();
    const level = this.level ?? autoLevel;
    const hasDetail = Boolean(selection?.item || selection?.artifact || selection?.artifactRefusal || selection?.itemRefusal || this.map?.lastPick);
    // The detail region shows whichever changed last.
    const showArtifact = this.lastDetail === 'artifact' && (selection?.artifact || selection?.artifactRefusal);
    const detail = html`<slot name="detail">${showArtifact ? html`<tessera-artifact-card exportparts=${FORWARD['artifact-card']}></tessera-artifact-card>` : html`<tessera-item-card exportparts=${FORWARD['item-card']} title-field=${this.titleField || nothing} .pick=${this.map?.lastPick ?? null}></tessera-item-card>`}</slot>`;
    // The view pickers draw nothing for a one-view bundle, so their row is left out there.
    // The pickers draw nothing for a one-view bundle, so their row is hidden there unless the host
    // filled the slot. The slot is always rendered, so its slotchange keeps `toolbarFilled` current.
    const pickersShown = this.toolbarFilled || (meta !== null && !hasOneLayout(meta));
    const pickers = html`<div class="pickers" ?hidden=${!pickersShown}><slot name="toolbar" @slotchange=${this.onToolbarSlot}><tessera-view-picker exportparts=${FORWARD['view-picker']}></tessera-view-picker><tessera-key-picker exportparts=${FORWARD['key-picker']}></tessera-key-picker></slot></div>`;
    const colour = html`<slot name="colour"><tessera-legend exportparts=${FORWARD.legend} selectable readout ?hide-palettes=${this.hidePalettes} .limit=${compact ? 4 : 0} .level=${this.level} .autoLevel=${autoLevel} @tessera-levelchange=${(e: CustomEvent<{level: number | null}>) => (this.level = e.detail.level)}></tessera-legend></slot>`;
    const layersPanel = html`<slot name="layers"><tessera-layer-picker exportparts=${FORWARD['layer-picker']}></tessera-layer-picker></slot>`;
    // Only the card's copy carries the id its Filters button controls, so no id repeats.
    const filters = (chipsOnly: boolean, id: string | typeof nothing = nothing) => html`<div id=${id} class="filters"><slot name="filters"><tessera-filter-panel exportparts=${FORWARD['filter-panel']} ?chips-only=${chipsOnly}></tessera-filter-panel></slot></div>`;
    const hierarchy = html`<slot name="hierarchy"><tessera-hierarchy exportparts=${FORWARD.hierarchy}></tessera-hierarchy></slot>`;
    // Drawn only where there is a hierarchy to walk; an empty section would look broken.
    const hasHierarchy = browsableLayers(meta?.layers ?? []).length > 0;
    const list = html`<slot name="artifacts"><tessera-artifact-list exportparts=${FORWARD['artifact-list']}></tessera-artifact-list></slot>`;
    const selectionPanel = this.has('selection') && region ? html`<slot name="selection"><tessera-selection exportparts=${FORWARD.selection}></tessera-selection></slot>` : nothing;
    const section = (name: string, title: string, summary: string, body: unknown) =>
      html`<details><summary><span class="t"><span class="closed-chev">${icon('chevr', 14)}</span><span class="open-chev">${icon('chev', 14)}</span>${title}</span><span class="summary">${summary}</span></summary><div class="body" data-section=${name}>${body}</div></details>`;
    const sections = html`${this.has('hierarchy') && hasHierarchy ? section('hierarchy', 'Hierarchy', '', hierarchy) : nothing}
      ${this.has('artifacts') ? section('artifacts', 'In view', inView > 0 ? inView.toLocaleString('en-GB') : '', list) : nothing}`;

    const docked = html`<aside part="sidebar" aria-label="Explorer panels">
      ${this.has('toolbar') ? html`<div class="head" ?hidden=${!pickersShown}>${pickers}</div>` : nothing}
      ${this.has('toolbar') ? colour : nothing}
      ${this.has('filters') ? filters(false) : nothing}
      ${selectionPanel}
      ${sections}
    </aside>`;

    const filtersToggle = this.has('filters')
      ? html`<button part="filters-toggle" class="btn" type="button" aria-controls="filters" aria-expanded=${this.filtersOpen ? 'true' : 'false'} data-count=${applied} @click=${() => (this.filtersOpen = !this.filtersOpen)}>${icon('filter', 14, 1.3)}Filters${applied > 0 ? html`<span class="badge">${applied}</span>` : nothing}</button>`
      : nothing;
    const panel = html`<div part="panel" class="card" role="region" aria-label="Explorer panels">
      ${this.has('toolbar') || this.has('filters') ? html`<div class="head" ?hidden=${!(this.has('toolbar') && pickersShown) && !this.has('filters')}>${this.has('toolbar') ? pickers : nothing}${filtersToggle}</div>` : nothing}
      ${this.has('filters') ? filters(!this.filtersOpen, 'filters') : nothing}
      ${this.has('toolbar') ? colour : nothing}
      ${selectionPanel}
      ${sections}
    </div>`;

    const layersButton = this.has('legend')
      ? html`<div class="layers" slot=${compact ? 'bottom-left' : 'top-left'} @keydown=${this.onLayersKey}>
          <div class="group"><button part="layers-toggle" type="button" aria-label=${`Layers and display, ${layersOn} layers on`} aria-expanded=${this.layersOpen ? 'true' : 'false'}
            aria-controls="layers-popover" ?data-on=${layersOn > 0} @click=${() => (this.layersOpen ? this.closeLayers() : this.openLayers())}>${icon('layers', 16, 1.2)}${layersOn > 0 ? html`<span class="on"></span>` : nothing}</button></div>
          ${this.layersOpen ? html`<div part="layers-popover" id="layers-popover" class="card" role="dialog" aria-label="Layers and display">${this.displaySection()}${layersPanel}</div>` : nothing}
        </div>`
      : nothing;

    const tabs = TABS.filter((t) => this.has(t.panel));
    // One tab is in the page's tab order: the one focused last, else the open sheet's, else the first.
    const reachable = tabs.find((t) => t.sheet === this.tabFocus) ?? tabs.find((t) => t.sheet === this.sheet) ?? tabs[0];
    const tab = ({sheet, icon: ic, label}: (typeof TABS)[number]) =>
      html`<button type="button" role="tab" id=${`tab-${sheet}`} data-sheet=${sheet} tabindex=${reachable?.sheet === sheet ? '0' : '-1'}
        aria-selected=${this.sheet === sheet ? 'true' : 'false'} aria-controls=${this.sheet === sheet ? 'sheet' : nothing}
        @click=${() => {
          this.tabFocus = sheet;
          this.sheet = this.sheet === sheet ? null : sheet;
        }}>${icon(ic, 18)}${label}</button>`;
    // The filters sheet's primary action names the number it will produce.
    const matched = s?.get('view').matched;
    const matchedText = matched && matched.exact && s?.get('status').status === 'shown' ? `Show ${matched.value.toLocaleString('en-GB')} matching` : 'Show';
    const sheetFooter = html`<div class="sheet-footer">
      <button class="btn" type="button" @click=${() => {
        const meta = s?.get('meta');
        if (!s || !meta) return;
        s.setFilters(emptyDraft(meta.filterOperands));
        s.setMembers([]);
      }}>Clear</button>
      <button class="btn primary" type="button" @click=${() => (this.sheet = null)}>${matchedText}</button>
    </div>`;
    const sheetBody =
      this.sheet === 'filters'
        ? html`${filters(false)}${this.has('hierarchy') && hasHierarchy ? hierarchy : nothing}${sheetFooter}`
        : this.sheet === 'layers'
          ? html`<div class="head" ?hidden=${!pickersShown}>${pickers}</div>${colour}<div class="panel">${layersPanel}</div>`
          : this.sheet === 'artifacts'
            ? html`${selectionPanel}${list}`
            : this.sheet === 'detail'
              ? detail
              : nothing;

    // The tooltip slot is forwarded only when the host supplied one: a slot assigned another slot
    // counts as filled even when that slot is empty, which would hide the map's own tooltip.
    return html`<div part="frame" class=${`${floating ? 'floating' : 'docked'}${compact ? ' compact' : ''}`}
      @tessera-artifactfit=${(e: CustomEvent<{id: string}>) => this.map?.fitTo(BigInt(e.detail.id))}
      @tessera-viewfollow=${(e: CustomEvent<{view: string; x: number; y: number}>) => this.followItem(e.detail)}
      @tessera-close=${() => this.closeDetail()}>
      ${floating ? nothing : docked}
      <div class="stage">
        <tessera-map
          exportparts=${FORWARD.map}
          colour-by=${this.colourBy || nothing}
          layers=${this.layers || nothing}
          tooltip-fields=${this.tooltipFields}
          title-field=${this.titleField || nothing}
          budget=${this.budget || nothing}
          controls-corner=${compact ? 'bottom-left' : 'top-left'}
          .clusterLevel=${level}
          .noPoints=${this.noPoints}
          .radius=${this.radius}
          .pointOpacity=${this.pointOpacity}
          .density=${this.density}
          .densityColours=${this.densityColours}
          .densityStrength=${this.densityStrength}
          .categoryPalette=${this.categoryPalette}
          .ramp=${this.ramp}
          .rampScale=${this.rampScale}
          .rampReverse=${this.rampReverse}
          .valueColours=${this.valueColours}
          @tessera-viewchange=${() => this.requestUpdate()}
          @tessera-pick=${() => this.requestUpdate()}
          @click=${() => this.requestUpdate()}
        >
          ${layersButton}
          <div slot="top-right" class="right"><slot name="top-right"></slot>${this.has('detail') && hasDetail ? html`<div part="detail" class="card">${detail}</div>` : nothing}</div>
          <div slot="bottom-right" class="in-map-strip"><slot name="status"><tessera-status exportparts=${FORWARD.status} ?compact=${compact}></tessera-status></slot></div>
          ${this.querySelector('[slot="tooltip"]') ? html`<slot name="tooltip" slot="tooltip"></slot>` : nothing}
        </tessera-map>
        ${floating ? panel : nothing}
      </div>
      ${this.sheet && sheetBody !== nothing
        ? html`<div part="sheet" id="sheet" role="dialog" aria-labelledby=${`tab-${this.sheet}`} tabindex="-1" @keydown=${this.onSheetKey}>${sheetBody}</div>`
        : nothing}
      <div part="strip-row"><tessera-status exportparts=${FORWARD.status}></tessera-status></div>
      <div part="tabs" role="tablist" aria-label="Explorer panels" @keydown=${this.onTabKey}>${tabs.map(tab)}</div>
    </div>`;
  }

  /** The display settings as they stand. */
  private get display(): DisplaySettings {
    return {
      points: !this.noPoints,
      radius: this.radius,
      pointOpacity: this.pointOpacity,
      density: this.density,
      densityColours: this.densityColours || null,
      densityStrength: this.densityStrength
    };
  }

  /**
   * The Display section of the Layers popover, over the layer picker. The Size and Opacity sliders
   * show what the map drew last while the setting is unset, and moving one fixes it.
   */
  private displaySection(): TemplateResult {
    const s = this.display;
    const probe = this.map?.probe.timings;
    const radius = s.radius ?? (probe && probe.markRadius > 0 ? probe.markRadius : 1.5);
    const opacity = s.pointOpacity ?? (probe && probe.markAlpha > 0 ? probe.markAlpha : 0.7);
    const scheme = this.map?.drawnGround ?? 'light';
    const colours = drawnDensityColours(s.density, this.densityColours, s.points);
    const modeAt = DENSITY_MODES.findIndex((m) => m.mode === s.density);
    const ramped = s.density === 'smooth' || s.density === 'hex' || s.density === 'grid';
    const change = (patch: Partial<DisplaySettings>) => this.changeDisplay(patch);
    const number = (e: Event) => Number((e.target as HTMLInputElement).value);
    const choices = Object.keys(DENSITY_COLOUR_TITLES) as DensityColours[];
    const colourList = this.densityColoursOpen
      ? html`<div id="density-colour-list" class="ramp-list" role="radiogroup" aria-labelledby="colours-label">
          ${choices.map(
            (c, i) => html`<button type="button" role="radio" data-colours=${c} aria-checked=${c === colours ? 'true' : 'false'} tabindex=${c === colours ? '0' : '-1'}
              @click=${() => change({densityColours: c})}
              @keydown=${(e: KeyboardEvent) => radioKeys(e, choices.length, i, (j) => change({densityColours: choices[j]!}))}><span class="bar" style=${`background:${densityGradient(c, scheme)}`}></span>${DENSITY_COLOUR_TITLES[c]}</button>`
          )}
        </div>`
      : nothing;
    const densityControls =
      s.density === 'none'
        ? html`<div class="gap"></div>`
        : html`<div class="sliders">
            ${ramped
              ? html`<span id="colours-label">Colours</span>
                  <button part="density-colours" class="ramp-choice" type="button" aria-labelledby="colours-label" aria-expanded=${this.densityColoursOpen ? 'true' : 'false'} aria-controls="density-colour-list"
                    @click=${() => (this.densityColoursOpen = !this.densityColoursOpen)}><span class="bar" style=${`background:${densityGradient(colours, scheme)}`}></span>${DENSITY_COLOUR_TITLES[colours]}${icon('chev', 12, 1.4)}</button>
                  ${colourList}`
              : nothing}
            <label for="density-strength">Strength</label><input id="density-strength" part="density-strength" type="range" min="0.1" max="1" step="0.05" .value=${String(s.densityStrength)}
              @input=${(e: Event) => change({densityStrength: number(e)})} />
          </div>`;
    return html`<div part="display" class="display">
      <div class="hd">Display</div>
      <div class="line">
        <span class="lead" id="points-label">Points</span>
        <button part="points-toggle" class="switch" type="button" role="switch" aria-checked=${s.points ? 'true' : 'false'} aria-labelledby="points-label"
          @click=${() => change({points: !s.points})}><span class="knob"></span></button>
      </div>
      <div class="sliders">
        <label for="point-size">Size</label><input id="point-size" part="point-size" type="range" min="0.5" max="8" step="0.5" .value=${String(radius)} ?disabled=${!s.points}
          @input=${(e: Event) => change({radius: number(e)})} />
        <label for="point-opacity">Opacity</label><input id="point-opacity" part="point-opacity" type="range" min="0.1" max="1" step="0.05" .value=${String(opacity)} ?disabled=${!s.points}
          @input=${(e: Event) => change({pointOpacity: number(e)})} />
      </div>
      <div class="rule"></div>
      <div class="lead" id="density-label">Density</div>
      <div part="density-mode" class="modes" role="radiogroup" aria-labelledby="density-label">
        ${DENSITY_MODES.map(
          (m, i) => html`<button type="button" role="radio" data-mode=${m.mode} aria-checked=${m.mode === s.density ? 'true' : 'false'} tabindex=${i === modeAt ? '0' : '-1'}
            @click=${() => change({density: m.mode})}
            @keydown=${(e: KeyboardEvent) => radioKeys(e, DENSITY_MODES.length, i, (j) => change({density: DENSITY_MODES[j]!.mode}))}>${icon(m.icon, 16, 1.3)}<span>${m.label}</span></button>`
        )}
      </div>
      ${densityControls}
    </div>`;
  }

  /** Apply a change made in the Display section, and report every setting as it now stands. */
  private changeDisplay(patch: Partial<DisplaySettings>): void {
    if (patch.points !== undefined) this.noPoints = !patch.points;
    if (patch.radius !== undefined) this.radius = patch.radius;
    if (patch.pointOpacity !== undefined) this.pointOpacity = patch.pointOpacity;
    if (patch.density !== undefined) this.density = patch.density;
    if (patch.densityColours !== undefined) {
      this.densityColours = patch.densityColours ?? '';
      this.densityColoursOpen = false;
    }
    if (patch.densityStrength !== undefined) this.densityStrength = patch.densityStrength;
    emit(this, 'tessera-displaychange', this.display);
  }

  /** Escape closes the Layers popover and returns focus to its button. */
  private onLayersKey = (e: KeyboardEvent): void => {
    if (e.key !== 'Escape' || !this.layersOpen) return;
    e.stopPropagation();
    this.closeLayers();
    this.renderRoot.querySelector<HTMLElement>('[part="layers-toggle"]')?.focus();
  };

  /** The tab list's keys: the arrows move between tabs, wrapping, and Home and End go to the ends. */
  private onTabKey = (e: KeyboardEvent): void => {
    const buttons = Array.from(this.renderRoot.querySelectorAll<HTMLButtonElement>('[part="tabs"] [role="tab"]'));
    const at = buttons.indexOf(e.target as HTMLButtonElement);
    if (at < 0) return;
    const n = buttons.length;
    const next = {ArrowRight: (at + 1) % n, ArrowLeft: (at - 1 + n) % n, Home: 0, End: n - 1}[e.key];
    if (next === undefined) return;
    e.preventDefault();
    const target = buttons[next]!;
    this.tabFocus = target.dataset.sheet as Sheet;
    target.focus();
  };

  /** Escape closes the sheet. */
  private onSheetKey = (e: KeyboardEvent): void => {
    if (e.key !== 'Escape') return;
    e.stopPropagation();
    this.sheet = null;
  };

  /** Focus goes into a sheet as it opens, and back to its tab as it closes. */
  protected override updated(changed: PropertyValues<this>): void {
    if (!changed.has('sheet')) return;
    const before = changed.get('sheet');
    if (this.sheet) this.renderRoot.querySelector<HTMLElement>('[part="sheet"]')?.focus();
    else if (before) this.renderRoot.querySelector<HTMLElement>(`[role="tab"][data-sheet="${before}"]`)?.focus();
  }

  /** The level a levelled layer draws at when none was chosen; see `render`. */
  private autoLevel(): number | null {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    const a = s?.get('artifacts');
    if (!s || !meta || !a) return null;
    const coloured = clusterLayerOf(s.get('legend').colourBy);
    const layer = coloured ?? a.layers[0] ?? null;
    const declared = layer ? meta.layers.find((l) => l.name === layer) : null;
    if (!declared || declared.levels.length === 0) return null;
    // On a levelled layer, the only kind reaching here, the served `rung` is the declared level.
    const counts: number[] = [];
    for (const x of coloured ? a.colourServed : a.served) {
      if (x.layer !== layer) continue;
      counts[x.rung] = (counts[x.rung] ?? 0) + 1;
    }
    for (let i = 0; i < counts.length; i++) counts[i] ??= 0;
    if (counts.length <= 1) return null;
    return levelForBudget(counts, artifactBudgetFor(this.map?.zoom ?? 0));
  }

  /** A follow in flight: dropped when it lands, when another starts, and at dispose. */
  private following: (() => void) | null = null;

  /**
   * Follow an item into another view: switch, then centre the camera on the position the item's
   * detail holds for that view. Across frames the map refits under the new view, so the camera
   * waits for the new view's first composition; within one frame it moves at once.
   */
  private followItem(detail: {view: string; x: number; y: number}): void {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    if (!s || !meta) return;
    this.following?.();
    this.following = null;
    const target = meta.views.find((v) => v.id === detail.view);
    if (!target) return;
    const centre = () => void this.updateComplete.then(() => this.map?.lookAt(detail.x, detail.y));
    if (s.get('view').id === detail.view) {
      centre();
      return;
    }
    const held = sameFrame(s.frame(), target.quantisation);
    s.setCurrentView(detail.view);
    if (held) {
      centre();
      return;
    }
    const stop = s.subscribe(() => {
      const view = s.get('view');
      // Another switch took over while this one waited.
      if (view.id !== detail.view) {
        this.following?.();
        return;
      }
      if (!view.composition) {
        // A refusal, an expiry or an empty view is final: no composition will come to centre on.
        const status = s.get('status');
        if (status.status === 'refused' || status.status === 'empty' || status.expired) this.following?.();
        return;
      }
      this.following?.();
      centre();
    });
    this.following = () => {
      stop();
      this.following = null;
    };
  }

  /** The card's close button: drop the selection it shows. */
  private closeDetail(): void {
    const s = this.resolvedStore;
    if (!s) return;
    const m = this.map;
    if (m) m.lastPick = null;
    s.clearSelection();
    this.requestUpdate();
  }
}

attachContextRoot();
defineOnce('tessera-explorer', TesseraExplorer);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-explorer': TesseraExplorer;
  }
}
