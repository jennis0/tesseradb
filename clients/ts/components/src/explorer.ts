import {ContextProvider} from '@lit/context';
import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import type {ClauseVerb, Store} from '@tesseradb/client';
import type {CategoryPaletteName, Colouring, DensityColours, DensityMode, RampName, RampScale, SizeScale, Sizing} from '@tesseradb/deck';
import {activeCount, browsableLayers, emptyDraft} from '@tesseradb/client';
import {artifactBudgetFor, hasOneLayout, levelForBudget, sizesPoints} from '@tesseradb/client/internal';
import {DENSITY_COLOUR_TITLES, clusterLayerOf} from '@tesseradb/deck/internal';
import './hierarchy.js';
import {TesseraElement, columnCaption, emit} from './base.js';
import {sizingOf} from './colouring.js';
import {densityGradient, displayStyles, radioKeys, type DisplaySettings} from './display.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon, type IconName} from './icons.js';
import {exportparts, forwarded} from './parts.js';
import {sameFrame} from './view-switch.js';
import {drawnDensityColours, type TesseraMap} from './map.js';
import type {TesseraFilterPanel} from './filter-panel.js';
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
/** The Scale choices under Size by, in order. */
const SIZE_SCALES: readonly {scale: SizeScale; label: string}[] = [
  {scale: 'linear', label: 'Linear'},
  {scale: 'log', label: 'Log'},
  {scale: 'rank', label: 'Rank'}
];
/** The range of the size sliders, radii in pixels. */
const SIZE_RANGE = {min: 0.5, max: 12, step: 0.5} as const;
/** A radius as the popover shows it. */
const px = (n: number) => n.toLocaleString('en-GB', {maximumFractionDigits: 1});

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
 * The heading names what is shown: `dataset-title` where the host sets one, with the view's name
 * under it, else the view's name alone. With several views the view's name is the view choice.
 *
 * `layout="docked"` puts a sidebar left of the map: the heading, then Colour (the legend), then
 * Filters (the applied clauses as chips, with Clear all, the Filter / Highlight switch and the
 * filter controls), and Hierarchy and In view as sections that start collapsed.
 * `layout="overlay"` folds the same content into one card over the map's top-left: the heading and
 * a Filters button with the number of clauses applied, the chips, then Colour. The Filters button
 * opens the switch and the controls in a panel beside the card, and the map's tools move to its
 * right while it is open; its close button, the Filters button and Escape close it. In both, the
 * map's tools sit at the top-left of the map (beside the card in the overlay layout) with a Layers
 * button beneath them. The map's top-right corner holds the selection while a region is selected
 * and, under it, the detail card; the status strip sits in its bottom-right. The Layers button
 * opens a popover with a Display section (whether the points are drawn, what sizes them, their
 * opacity, and how density is drawn, in which colours and how strongly) over the layer picker.
 * Size by lists None and the number columns the points arrive with. Under None one Size slider
 * sets every point's radius, which `hide-size` leaves out; under a column two sliders set the
 * radii of its smallest and largest value, and a Linear, Log or Rank choice places values between
 * them. The legend then gains a Size section. The display settings are the explorer's properties
 * of the same names, passed to its map. `pinned-filters` names the columns whose controls are
 * listed before they hold a clause.
 *
 * In a container narrower than 1000 px, either layout folds into the card, 300 px wide, whose
 * legend names four values and offers the rest under "N more"; the tools move to the bottom-left,
 * the strip shortens its figures, and the filter panel opens beside the card over the map. In a
 * container 720 px wide or narrower, the status strip runs full width above a tab bar (Filters,
 * Layers, In view, Item), and each tab opens its panel as a sheet. The Hierarchy section appears
 * only where the bundle has a hierarchical layer, and the detail card only once something is
 * selected.
 *
 * Every event its elements fire bubbles out of it, since each is composed.
 *
 * @summary The map and every panel, in a default layout.
 * @tagname tessera-explorer
 * @category Elements
 * @slot toolbar - Replaces the view picker and the key picker.
 * @slot colour - Replaces the legend.
 * @slot layers - Replaces the layer picker, in the Layers popover.
 * @slot filters - Replaces the filter panel: the chips and the controls in the sidebar, the
 *   controls in the overlay's panel, whose card then shows no chips of its own.
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
 * @fires {CustomEvent<TesseraEventDetails['tessera-sizechange']>} tessera-sizechange - Size by, the size range or the scale changed in the Display section.
 * @fires {CustomEvent<TesseraEventDetails['tessera-valuecolour']>} tessera-valuecolour - A colour was chosen or reset for one value in the legend.
 * @fires {CustomEvent<TesseraEventDetails['tessera-palettechange']>} tessera-palettechange - The palette or ramp was chosen in the legend.
 * @fires {CustomEvent<TesseraEventDetails['tessera-statechange']>} tessera-statechange - The status strip's panel state changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-expired']>} tessera-expired - The session expired.
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A filter control or chip changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-clausechange']>} tessera-clausechange - A `member_of` clause was put on or taken off.
 * @fires {CustomEvent<TesseraEventDetails['tessera-chipopen']>} tessera-chipopen - A filter chip was pressed while the controls were closed, which opens them.
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
 * @csspart dataset-title - The heading's dataset title, where `dataset-title` is set.
 * @csspart view-name - The view's name in the heading, where there is one view to show.
 * @csspart filters-toggle - The card's Filters button, with `aria-expanded` while the controls show
 *   and `data-count` set to the number of clauses applied.
 * @csspart filters-popover - The panel beside the card holding the filter controls, while it is
 *   open.
 * @csspart filters-close - The close button of that panel.
 * @csspart selection-card - The card in the map's top-right corner holding the selection, while a
 *   region is selected.
 * @csspart layers-toggle - The Layers button under the map's tools, with `data-on` while any layer
 *   is drawn.
 * @csspart layers-popover - The popover holding the Display section and the layer picker, while it
 *   is open.
 * @csspart display - The Display section at the top of the Layers popover.
 * @csspart points-toggle - The Points switch, with `aria-checked`.
 * @csspart size-by - The Size by button, naming the column the points are sized by or None, with
 *   `aria-expanded` while its menu is open.
 * @csspart size-menu - The Size by menu, while it is open.
 * @csspart size-option - An entry in the Size by menu, with `data-value` (empty for None) and
 *   `aria-checked`.
 * @csspart point-size - The Size slider: every point's radius in pixels, under Size by None unless
 *   `hide-size` is set.
 * @csspart size-min - The slider for the radius of the smallest value, under Size by a column.
 * @csspart size-max - The slider for the radius of the largest value, under Size by a column.
 * @csspart size-range - The two radii as text, such as "2 – 9 px".
 * @csspart size-scale - The Linear, Log and Rank choice, each with `data-scale` and `aria-checked`.
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
      /* The compact form's corners are nearer the edges. */
      :host([data-compact]) {
        --_tessera-space-base: 12px;
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
        --_tessera-tool-size: 30px;
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
      /* What the map draws at its top-left, its tools or a region's tag, sits right of the card, and
         of the filter panel while it is open; in the narrow layout there is no card. */
      @container explorer (width > 720px) {
        .floating tessera-map {
          --tessera-map-inset-left: calc(var(--_panel-width) + var(--_tessera-space));
        }
        .floating.filters-open tessera-map {
          --tessera-map-inset-left: calc(var(--_panel-width) + 8px + 360px + var(--_tessera-space));
        }
      }
      [part='sidebar'] {
        display: flex;
        flex-direction: column;
        overflow-y: auto;
        border-right: 1px solid var(--_tessera-line);
        background: var(--_tessera-surface);
        --_tessera-panel-rule: var(--_tessera-line);
      }
      [part='sidebar'] .head {
        border-bottom-color: var(--_tessera-line);
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
        --_tessera-panel-padding: 12px 14px 14px;
      }
      [part='panel'] tessera-filter-panel {
        --_tessera-panel-padding: 12px 14px;
      }
      .compact [part='panel'] {
        --_tessera-panel-padding: 10px 12px;
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
      .names {
        display: flex;
        flex-direction: column;
        min-width: 0;
      }
      .names .pickers {
        display: flex;
        flex-direction: column;
        gap: 8px;
        min-width: 0;
      }
      .title {
        font-size: 15px;
        font-weight: 600;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      .compact .title {
        font-size: 13px;
      }
      .sub {
        font-size: 12px;
        color: var(--_tessera-ink-2);
      }
      .sub .pickers {
        gap: 2px;
      }
      .sub tessera-view-picker::part(field) {
        font-size: 12px;
        font-weight: 400;
        line-height: 1.45;
      }
      .compact tessera-view-picker::part(field) {
        font-size: 13px;
      }
      .compact .sub tessera-view-picker::part(field) {
        font-size: 12px;
      }
      [part='filters-toggle'] {
        margin-left: auto;
        height: auto;
        padding: 5px 9px 5px 8px;
      }
      .compact [part='filters-toggle'] {
        height: 28px;
        padding: 0 8px;
        border: 0;
      }
      [part='filters-toggle'][aria-expanded='true'] {
        background: var(--_tessera-accent);
        border-color: var(--_tessera-accent);
        color: var(--_tessera-accent-ink);
      }
      [part='filters-toggle'][aria-expanded='true'] .badge {
        background: var(--_tessera-accent-ink);
        color: var(--_tessera-accent);
      }
      /* The filter controls in a panel beside the card, with the map's tools moved to its right. */
      [part='filters-popover'] {
        --_tessera-panel-inline: 14px;
        position: absolute;
        z-index: 2;
        top: var(--_tessera-space);
        left: calc(var(--_tessera-space) + var(--_panel-width) + 8px);
        width: min(360px, calc(100% - var(--_panel-width) - 2 * var(--_tessera-space) - 8px));
        max-height: calc(100% - 2 * var(--_tessera-space));
        overflow-y: auto;
        box-shadow: 0 6px 24px rgba(0, 0, 0, 0.1);
      }
      [part='filters-close'] {
        position: absolute;
        z-index: 1;
        top: 10px;
        right: 10px;
        width: 26px;
        height: 26px;
        display: grid;
        place-items: center;
        border-radius: 5px;
        color: var(--_tessera-ink-2);
      }
      [part='filters-close']:hover {
        background: var(--_tessera-surface-2);
      }
      [part='filters-popover'][hidden] {
        display: none;
      }
      /* In the compact form the filter panel lies over the map's right-hand cards, so they give way. */
      .compact.filters-open .right {
        display: none;
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
      [part='panel'] tessera-filter-panel::part(title) {
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
        width: var(--_tessera-tool-size, 32px);
        height: var(--_tessera-tool-size, 32px);
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
      [part='detail'],
      [part='selection-card'] {
        --_tessera-panel-padding: 14px 14px 12px;
        width: 320px;
        max-width: 100%;
        min-height: 0;
        overflow-y: auto;
      }
      [part='detail'] {
        flex: 0 1 auto;
      }
      /* The selection keeps its heading, counts and actions; only its list of marks scrolls, so the
         detail card under it stays on screen. */
      [part='selection-card'] {
        flex: none;
        overflow: visible;
      }
      [part='selection-card'] tessera-selection::part(items) {
        max-height: min(144px, 18cqh);
        overflow-y: auto;
      }
      .compact [part='selection-card'] {
        width: 272px;
      }
      .compact [part='detail'] tessera-item-card,
      .compact [part='detail'] tessera-artifact-card {
        font-size: 12px;
      }
      .compact [part='detail'] {
        width: 272px;
        --_tessera-panel-padding: 12px 12px 10px;
        --_tessera-title-size: 14px;
        --_tessera-key-width: 76px;
        --_tessera-field-gap: 4px 10px;
      }
      .right {
        display: flex;
        flex-direction: column;
        align-items: flex-end;
        gap: calc(var(--_tessera-space) / 2);
        max-height: calc(100cqh - 2 * var(--_tessera-space) - 48px);
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
        [part='filters-popover'],
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
  /** Passed to the map's `size-by`; the Size by choice in the Layers popover changes it. */
  @property({attribute: 'size-by'}) accessor sizeBy = '';
  /** Passed to the map's `size-min`; the smallest-size slider in the Layers popover changes it. */
  @property({type: Number, attribute: 'size-min'}) accessor sizeMin: number | null = null;
  /** Passed to the map's `size-max`; the largest-size slider in the Layers popover changes it. */
  @property({type: Number, attribute: 'size-max'}) accessor sizeMax: number | null = null;
  /** Passed to the map's `size-scale`; the Scale choice in the Layers popover changes it. */
  @property({attribute: 'size-scale'}) accessor sizeScale: SizeScale | '' = '';
  /** Leaves the Size slider out of the Layers popover, for a host that sets `radius` itself. */
  @property({type: Boolean, attribute: 'hide-size'}) accessor hideSize = false;
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
  /** The dataset's title, which heads the explorer above the view's name. Unset, the view's name is the heading. */
  @property({attribute: 'dataset-title'}) accessor datasetTitle = '';
  /** Passed to the filter panel's `pinned`: the columns whose controls are listed before they hold a clause. */
  @property({attribute: 'pinned-filters'}) accessor pinnedFilters = '';
  /** Passed to the legend's `hide-palettes`: the palette and ramp choices are left out of Colour by. */
  @property({type: Boolean, attribute: 'hide-palettes'}) accessor hidePalettes = false;
  /** Whether the Size by menu in the Layers popover is open. @internal */
  @state() accessor sizeMenuOpen = false;
  /** Whether the list of density colours in the Layers popover is open. @internal */
  @state() accessor densityColoursOpen = false;
  /** @internal */
  @state() accessor sheet: Sheet | null = null;
  /** The narrow layout's tab focused last, which keeps the tab list's one place in the tab order. */
  @state() private accessor tabFocus: Sheet | null = null;
  /** The level chosen through the legend's select; the map colours and labels at it. @internal */
  @state() accessor level: number | null = null;
  /** Whether the card's Filters button has opened the panel of controls. @internal */
  @state() accessor filtersOpen = false;
  /**
   * Where focus goes once the panel of controls shows: the control a chip asked for, or the panel
   * itself where the Filters button opened it.
   */
  private showing: {column: string; verb: ClauseVerb} | 'panel' | null = null;
  /** Whether the Layers popover is open. @internal */
  @state() accessor layersOpen = false;
  /** Whether the container is narrower than 1000 px and wider than the narrow layout. @internal */
  @state() accessor compact = false;
  /** Whether the host put anything in the filters slot, in place of the filter panel and its chips. */
  @state() private accessor filtersFilled = false;

  private onFiltersSlot = (e: Event): void => {
    this.filtersFilled = (e.target as HTMLSlotElement).assignedElements().length > 0;
  };

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
      // The narrow layout shows the filters as a sheet, so a panel left open beside the card closes.
      if (width <= COMPACT_BETWEEN[0]) this.filtersOpen = false;
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
    this.sizeMenuOpen = false;
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
    // The heading names what is shown: the dataset's title over the view's name, or the view's
    // name alone. With one view the name is text; with several it is the view choice.
    const shown = meta?.views.find((v) => v.id === s?.get('view').id) ?? (meta?.views.length === 1 ? meta.views[0] : undefined);
    const viewName = shown?.displayName ?? '';
    const viewText = (cls: string) => (pickersShown || !viewName ? nothing : html`<span part="view-name" class=${cls}>${viewName}</span>`);
    const names = this.datasetTitle
      ? html`<div class="names"><span part="dataset-title" class="title">${this.datasetTitle}</span><div class="sub">${pickers}${viewText('')}</div></div>`
      : html`<div class="names">${pickers}${viewText('title')}</div>`;
    const named = pickersShown || viewName !== '' || this.datasetTitle !== '';
    const colour = html`<slot name="colour"><tessera-legend exportparts=${FORWARD.legend} selectable readout ?hide-palettes=${this.hidePalettes} .limit=${compact ? 4 : 0} .level=${this.level} .autoLevel=${autoLevel} @tessera-levelchange=${(e: CustomEvent<{level: number | null}>) => (this.level = e.detail.level)}></tessera-legend></slot>`;
    const layersPanel = html`<slot name="layers"><tessera-layer-picker exportparts=${FORWARD['layer-picker']}></tessera-layer-picker></slot>`;
    // Only the card's copy carries the id its Filters button controls, so no id repeats.
    const filters = html`<slot name="filters" @slotchange=${this.onFiltersSlot}><tessera-filter-panel exportparts=${FORWARD['filter-panel']} pinned=${this.pinnedFilters || nothing}></tessera-filter-panel></slot>`;
    // The card's chips; a chip pressed opens the panel of controls at the chip's control.
    const chips = this.filtersFilled
      ? nothing
      : html`<tessera-filter-panel exportparts=${FORWARD['filter-panel']} chips-only @tessera-chipopen=${(e: CustomEvent<{column: string; verb: ClauseVerb}>) => this.openFilters(e.detail)}></tessera-filter-panel>`;
    const hierarchy = html`<slot name="hierarchy"><tessera-hierarchy exportparts=${FORWARD.hierarchy}></tessera-hierarchy></slot>`;
    // Drawn only where there is a hierarchy to walk; an empty section would look broken.
    const hasHierarchy = browsableLayers(meta?.layers ?? []).length > 0;
    const list = html`<slot name="artifacts"><tessera-artifact-list exportparts=${FORWARD['artifact-list']}></tessera-artifact-list></slot>`;
    const selectionPanel = this.has('selection') && region ? html`<slot name="selection"><tessera-selection exportparts=${FORWARD.selection}></tessera-selection></slot>` : nothing;
    const selectionCard = selectionPanel === nothing ? nothing : html`<div part="selection-card" class="card">${selectionPanel}</div>`;
    const section = (name: string, title: string, summary: string, body: unknown) =>
      html`<details><summary><span class="t"><span class="closed-chev">${icon('chevr', 14)}</span><span class="open-chev">${icon('chev', 14)}</span>${title}</span><span class="summary">${summary}</span></summary><div class="body" data-section=${name}>${body}</div></details>`;
    const sections = html`${this.has('hierarchy') && hasHierarchy ? section('hierarchy', 'Hierarchy', '', hierarchy) : nothing}
      ${this.has('artifacts') ? section('artifacts', 'In view', inView > 0 ? inView.toLocaleString('en-GB') : '', list) : nothing}`;

    const docked = html`<aside part="sidebar" aria-label="Explorer panels">
      ${this.has('toolbar') ? html`<div class="head" ?hidden=${!named}>${names}</div>` : nothing}
      ${this.has('toolbar') ? colour : nothing}
      ${this.has('filters') ? filters : nothing}
      ${sections}
    </aside>`;

    const filtersToggle = this.has('filters')
      ? html`<button part="filters-toggle" class="btn" type="button" aria-controls=${this.filtersOpen ? 'filters' : nothing} aria-expanded=${this.filtersOpen ? 'true' : 'false'} data-count=${applied} @click=${() => (this.filtersOpen ? this.closeFilters() : this.openFilters(null))}>${icon('filter', 14, 1.3)}Filters${applied > 0 ? html`<span class="badge">${applied}</span>` : nothing}</button>`
      : nothing;
    const panel = html`<div part="panel" class="card" role="region" aria-label="Explorer panels">
      ${this.has('toolbar') || this.has('filters') ? html`<div class="head" ?hidden=${!(this.has('toolbar') && named) && !this.has('filters')}>${this.has('toolbar') ? names : nothing}${filtersToggle}</div>` : nothing}
      ${this.has('filters') ? chips : nothing}
      ${this.has('toolbar') ? colour : nothing}
      ${sections}
    </div>`;
    // Drawn while closed too, so its switch and the fields opened in it last while the explorer does.
    const filtersPopover = this.has('filters')
      ? html`<div part="filters-popover" id="filters" class="card" role="dialog" aria-label="Filters" ?hidden=${!this.filtersOpen} @keydown=${this.onFiltersKey}>
            <button part="filters-close" type="button" aria-label="Close filters" @click=${() => this.closeFilters()}>${icon('close', 14, 1.4)}</button>
            <slot name="filters" @slotchange=${this.onFiltersSlot}><tessera-filter-panel exportparts=${FORWARD['filter-panel']} controls-only pinned=${this.pinnedFilters || nothing}></tessera-filter-panel></slot>
          </div>`
        : nothing;

    const layersButton = this.has('legend')
      ? html`<div class="layers" slot=${compact ? 'bottom-left' : 'top-left'} @keydown=${this.onLayersKey}>
          <div class="group"><button part="layers-toggle" type="button" aria-label=${`Layers and display, ${layersOn} layers on`} aria-expanded=${this.layersOpen ? 'true' : 'false'}
            aria-controls="layers-popover" ?data-on=${layersOn > 0} @click=${() => (this.layersOpen ? this.closeLayers() : this.openLayers())}>${icon('layers', 16, 1.2)}${layersOn > 0 ? html`<span class="on"></span>` : nothing}</button></div>
          ${this.layersOpen
            ? html`<div part="layers-popover" id="layers-popover" class="card" role="dialog" aria-label="Layers and display" @pointerdown=${this.onPopoverPress}>${this.displaySection()}${layersPanel}</div>${this.sizeMenu()}`
            : nothing}
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
        ? html`${filters}${this.has('hierarchy') && hasHierarchy ? hierarchy : nothing}${sheetFooter}`
        : this.sheet === 'layers'
          ? html`<div class="head" ?hidden=${!named}>${names}</div>${colour}<div class="panel">${layersPanel}</div>`
          : this.sheet === 'artifacts'
            ? html`${selectionPanel}${list}`
            : this.sheet === 'detail'
              ? detail
              : nothing;

    // The tooltip slot is forwarded only when the host supplied one: a slot assigned another slot
    // counts as filled even when that slot is empty, which would hide the map's own tooltip.
    return html`<div part="frame" class=${`${floating ? 'floating' : 'docked'}${compact ? ' compact' : ''}${floating && this.filtersOpen ? ' filters-open' : ''}`}
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
          size-by=${this.sizeBy || nothing}
          .sizeMin=${this.sizeMin}
          .sizeMax=${this.sizeMax}
          .sizeScale=${this.sizeScale}
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
          <div slot="top-right" class="right"><slot name="top-right"></slot>${selectionCard}${this.has('detail') && hasDetail ? html`<div part="detail" class="card">${detail}</div>` : nothing}</div>
          <div slot="bottom-right" class="in-map-strip"><slot name="status"><tessera-status exportparts=${FORWARD.status} ?compact=${compact}></tessera-status></slot></div>
          ${this.querySelector('[slot="tooltip"]') ? html`<slot name="tooltip" slot="tooltip"></slot>` : nothing}
        </tessera-map>
        ${floating ? panel : nothing}
        ${floating ? filtersPopover : nothing}
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
    const radius = s.radius ?? (probe && probe.markRadius > 0 ? probe.markRadius : 2);
    const opacity = s.pointOpacity ?? (probe && probe.markAlpha > 0 ? probe.markAlpha : 0.8);
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
        ${this.sizeControls(radius, !s.points)}
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

  /** The number column the points are sized by, or null for one size: the explorer's own choice where it made one, else the store's. */
  private get sizedBy(): string | null {
    if (this.sizeBy !== '') return this.sizeBy === 'none' ? null : this.sizeBy;
    return this.resolvedStore?.get('legend').sizeBy ?? null;
  }

  /** The size range and scale: the explorer's own where set, else the choices every element over the store shares. */
  private get sizing(): Sizing {
    const shared = sizingOf(this.resolvedStore);
    return {min: this.sizeMin ?? shared.min, max: this.sizeMax ?? shared.max, scale: this.sizeScale || shared.scale};
  }

  /** The columns Size by offers: the rendered number columns, in declaration order. */
  private get sizeColumns(): string[] {
    return (this.resolvedStore?.get('meta')?.declaredScalars ?? []).filter(sizesPoints).map((c) => c.name);
  }

  /**
   * Size by, then under None the Size slider (unless `hide-size`), or under a column the range of
   * radii and the scale. Rendered into the Display section's grid of labels and controls.
   */
  private sizeControls(radius: number, disabled: boolean): TemplateResult {
    const sizeBy = this.sizedBy;
    const sizing = this.sizing;
    const number = (e: Event) => Number((e.target as HTMLInputElement).value);
    const {min, max, step} = SIZE_RANGE;
    const choice = html`<span id="size-by-label">Size by</span>
      <button part="size-by" class="ramp-choice" type="button" aria-haspopup="menu" aria-expanded=${this.sizeMenuOpen ? 'true' : 'false'} aria-label=${`Size by: ${sizeBy === null ? 'None' : columnCaption(sizeBy)}`} ?disabled=${disabled}
        @click=${() => (this.sizeMenuOpen = !this.sizeMenuOpen)}><span class="t">${sizeBy === null ? 'None' : columnCaption(sizeBy)}</span>${icon('chev', 12, 1.4)}</button>`;
    if (sizeBy === null) {
      if (this.hideSize) return choice;
      return html`${choice}<label for="point-size">Size</label>
        <div class="with-readout"><input id="point-size" part="point-size" type="range" min=${min} max=${max} step=${step} .value=${String(radius)} ?disabled=${disabled}
          @input=${(e: Event) => this.changeDisplay({radius: number(e)})} /><span class="readout">${px(radius)} px</span></div>`;
    }
    const at = SIZE_SCALES.findIndex((x) => x.scale === sizing.scale);
    // Moving one end past the other takes the other with it.
    return html`${choice}<span id="size-range-label">Range</span>
      <div class="range" role="group" aria-labelledby="size-range-label">
        <input part="size-min" type="range" min=${min} max=${max} step=${step} aria-label="Smallest size" .value=${String(sizing.min)} ?disabled=${disabled}
          @input=${(e: Event) => this.changeSize({min: number(e), max: Math.max(number(e), sizing.max)})} />
        <span part="size-range" class="readout">${px(sizing.min)} – ${px(sizing.max)} px</span>
        <input part="size-max" type="range" min=${min} max=${max} step=${step} aria-label="Largest size" .value=${String(sizing.max)} ?disabled=${disabled}
          @input=${(e: Event) => this.changeSize({max: number(e), min: Math.min(number(e), sizing.min)})} />
      </div>
      <span id="size-scale-label">Scale</span>
      <div part="size-scale" class="seg" role="radiogroup" aria-labelledby="size-scale-label">
        ${SIZE_SCALES.map(
          (x, i) => html`<button type="button" role="radio" data-scale=${x.scale} aria-checked=${x.scale === sizing.scale ? 'true' : 'false'} tabindex=${i === at ? '0' : '-1'} ?disabled=${disabled}
            @click=${() => this.changeSize({scale: x.scale})}
            @keydown=${(e: KeyboardEvent) => radioKeys(e, SIZE_SCALES.length, i, (j) => this.changeSize({scale: SIZE_SCALES[j]!.scale}))}>${x.label}</button>`
        )}
      </div>`;
  }

  /** The Size by menu, beside the Layers popover while it is open: None and the number columns. */
  private sizeMenu(): TemplateResult | typeof nothing {
    if (!this.sizeMenuOpen) return nothing;
    const sizeBy = this.sizedBy;
    const options = [{value: '', title: 'None', kind: ''}, ...this.sizeColumns.map((c) => ({value: c, title: columnCaption(c), kind: 'Number'}))];
    const choose = (value: string) => {
      this.sizeMenuOpen = false;
      this.changeSize({sizeBy: value === '' ? null : value});
      void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="size-by"]')?.focus());
    };
    const keys = (e: KeyboardEvent, i: number) => {
      const items = Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="size-option"]'));
      const next = {ArrowDown: (i + 1) % items.length, ArrowUp: (i - 1 + items.length) % items.length, Home: 0, End: items.length - 1}[e.key];
      if (e.key === 'Escape') {
        e.stopPropagation();
        this.sizeMenuOpen = false;
        void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="size-by"]')?.focus());
        return;
      }
      if (next === undefined) return;
      e.preventDefault();
      items[next]?.focus();
    };
    return html`<div part="size-menu" class="size-menu" popover="manual" role="menu" aria-labelledby="size-menu-label">
      <div class="hd" id="size-menu-label">Size by</div>
      ${options.map(
        (o, i) => html`<button part="size-option" type="button" role="menuitemradio" data-value=${o.value} aria-checked=${(o.value || null) === sizeBy ? 'true' : 'false'}
          @click=${() => choose(o.value)} @keydown=${(e: KeyboardEvent) => keys(e, i)}><span>${o.title}</span><span class="kind">${o.kind}</span></button>`
      )}
    </div>`;
  }

  /** A press in the Layers popover outside the Size by button closes the Size by menu. */
  private onPopoverPress = (e: PointerEvent): void => {
    if (!this.sizeMenuOpen) return;
    const button = this.renderRoot.querySelector('[part="size-by"]');
    if (button && !e.composedPath().includes(button)) this.sizeMenuOpen = false;
  };

  /** Apply a change to Size by, the size range or the scale, and report all four as they now stand. */
  private changeSize(patch: {sizeBy?: string | null; min?: number; max?: number; scale?: SizeScale}): void {
    if (patch.sizeBy !== undefined) this.sizeBy = patch.sizeBy ?? 'none';
    if (patch.min !== undefined) this.sizeMin = patch.min;
    if (patch.max !== undefined) this.sizeMax = patch.max;
    if (patch.scale !== undefined) this.sizeScale = patch.scale;
    const {min, max, scale} = this.sizing;
    emit(this, 'tessera-sizechange', {sizeBy: this.sizedBy, min, max, scale});
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

  /** Open the panel of controls, at `at`'s control where a chip asked for one. */
  private openFilters(at: {column: string; verb: ClauseVerb} | null): void {
    this.filtersOpen = true;
    this.showing = at ?? 'panel';
  }

  private closeFilters(): void {
    this.filtersOpen = false;
    this.renderRoot.querySelector<HTMLElement>('[part="filters-toggle"]')?.focus();
  }

  /** Escape closes the panel of controls and returns focus to the Filters button. */
  private onFiltersKey = (e: KeyboardEvent): void => {
    if (e.key !== 'Escape') return;
    e.stopPropagation();
    this.closeFilters();
  };

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
    this.toggleAttribute('data-compact', this.compact);
    this.placeSizeMenu(changed.has('sizeMenuOpen'));
    if (this.showing) {
      const showing = this.showing;
      this.showing = null;
      const panel = this.renderRoot.querySelector<TesseraFilterPanel>('[part="filters-popover"] tessera-filter-panel');
      if (showing === 'panel') void panel?.updateComplete.then(() => panel.focus());
      else panel?.show(showing.column, showing.verb);
    }
    if (!changed.has('sheet')) return;
    const before = changed.get('sheet');
    if (this.sheet) this.renderRoot.querySelector<HTMLElement>('[part="sheet"]')?.focus();
    else if (before) this.renderRoot.querySelector<HTMLElement>(`[role="tab"][data-sheet="${before}"]`)?.focus();
  }

  /**
   * Show the Size by menu in the top layer, so the popover's scrolling does not clip it, beside the
   * popover and level with its button; focus goes to the entry chosen as it opens.
   */
  private placeSizeMenu(opened: boolean): void {
    const menu = this.renderRoot.querySelector<HTMLElement>('[part="size-menu"]');
    const button = this.renderRoot.querySelector<HTMLElement>('[part="size-by"]');
    const popover = this.renderRoot.querySelector<HTMLElement>('[part="layers-popover"]');
    if (!menu || !button || !popover) return;
    if (typeof menu.showPopover === 'function' && !menu.matches(':popover-open')) {
      try {
        menu.showPopover();
      } catch {
        // Shown already; the menu is in the page either way.
      }
    }
    const b = button.getBoundingClientRect();
    const p = popover.getBoundingClientRect();
    const width = menu.offsetWidth || 220;
    const height = menu.offsetHeight || 0;
    const vw = typeof innerWidth === 'number' ? innerWidth : 1024;
    const vh = typeof innerHeight === 'number' ? innerHeight : 768;
    const right = p.right + 16;
    const left = right + width <= vw - 8 ? right : Math.max(8, p.left - width - 16);
    menu.style.left = `${Math.round(left)}px`;
    menu.style.top = `${Math.round(Math.max(8, Math.min(b.top - 32, vh - height - 8)))}px`;
    if (opened) (menu.querySelector<HTMLElement>('[aria-checked="true"]') ?? menu.querySelector<HTMLElement>('button'))?.focus();
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
