import {ContextProvider} from '@lit/context';
import {css, html, nothing, svg, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import type {AggregateSpec, ArtifactDetail, ItemDetail, Store} from '@tesseradb/client';
import type {CategoryPaletteName, Colouring, DensityColours, DensityMode, DensityScale, RampName, RampScale, SizeScale, Sizing} from '@tesseradb/deck';
import {DEFAULT_DENSITY_CELL_PX, DEFAULT_DENSITY_SCALE, DENSITY_CELL_SIZES, cellDepth, nearestStop} from '@tesseradb/deck';
import {WORLD_SIZE, activeCount, artifactName, emptyDraft} from '@tesseradb/client';
import {artifactBudgetFor, hasOneLayout, levelForBudget, sizesPoints} from '@tesseradb/client/internal';
import {DENSITY_COLOUR_TITLES, clusterLayerOf} from '@tesseradb/deck/internal';
import {listedAt} from './artifact-list.js';
import {HeldAggregate} from './aggregate.js';
import {TesseraElement, columnCaption, emit} from './base.js';
import {placeCallout, type Rect, type Side} from './callout.js';
import {sizingOf, watchChoices} from './colouring.js';
import {densityGradient, displayStyles, radioKeys, type DisplaySettings} from './display.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon, type IconName} from './icons.js';
import {exportparts, forwarded} from './parts.js';
import {sameFrame} from './view-switch.js';
import {drawnDensityColours, drawnDensityScale, type TesseraMap} from './map.js';
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
  'filter-panel': exportparts('filter-panel', [...forwarded('filter'), ...forwarded('cluster-filter')]),
  'artifact-list': exportparts('artifact-list'),
  selection: exportparts('selection'),
  'item-card': exportparts('item-card'),
  'artifact-card': exportparts('artifact-card')
};

const ALL_PANELS = ['toolbar', 'legend', 'filters', 'artifacts', 'selection', 'detail'] as const;
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
/** The Scale choices under Resolution, in order. */
const DENSITY_SCALES: readonly {scale: DensityScale; label: string}[] = [
  {scale: 'linear', label: 'Linear'},
  {scale: 'log', label: 'Log'}
];
/** The range of the size sliders, radii in pixels. */
const SIZE_RANGE = {min: 1, max: 12, step: 0.5} as const;
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
/** A card kept open by Pin: what it shows, and where on the map it points, in the view it was pinned in. */
type Pinned = {key: string; world: [number, number]; view: string} & (
  | {kind: 'item'; item: {id: bigint; detail: ItemDetail}}
  | {kind: 'artifact'; artifact: {id: bigint; detail: ArtifactDetail}}
);
/** A card's size before it has been measured. */
const CALLOUT_SIZE = {width: 300, height: 220};
/** How long after the camera stops a callout may move to another side. */
const CALLOUT_REST_MS = 200;
/** The shortest a callout is made to fit beside its point; what does not fit scrolls inside it. */
const CALLOUT_MIN_HEIGHT = 180;

/** The narrow layout's tabs, each opening a sheet, drawn where its panel is. */
const TABS: readonly {sheet: Sheet; icon: IconName; label: string; panel: Panel}[] = [
  {sheet: 'filters', icon: 'filter', label: 'Filters', panel: 'filters'},
  {sheet: 'layers', icon: 'layers', label: 'Layers', panel: 'legend'},
  {sheet: 'artifacts', icon: 'list', label: 'In view', panel: 'artifacts'},
  {sheet: 'detail', icon: 'info', label: 'Item', panel: 'detail'}
];

/**
 * The map with its status strip, toolbar, layers, filters, colour legend, In view list, selection
 * and item card, laid out together: the controls on the left and what the map shows on the right.
 * It builds its own store from `viewer-url` and `token` or an `authorise` property, or takes a
 * `store` property, and provides it by context to everything inside it, including elements a host
 * puts in its slots.
 *
 * The heading names what is shown: `dataset-title` where the host sets one, with the view's name
 * under it, else the view's name alone. With several views the view's name is the view choice.
 *
 * The left holds the heading and the filter panel: the Filters heading with Clear all, the Filter /
 * Highlight switch with the number of clauses in each, the fields, and Add filter, which offers the
 * columns and the layers whose clusters can be filtered by. `layout="docked"` puts them in a
 * sidebar left of the map; `layout="overlay"` puts them in a card over the map's top-left, which
 * scrolls inside itself when tall. The map's tools sit at the top-left of the map, right of the
 * card in the overlay layout, with a Layers button beneath them.
 *
 * The right holds, over the map in both layouts, a card with Colour (the legend, with the Colour by
 * choice in its heading and exact counts beside the colours, then Size where the points are sized
 * by a column) and In view (the clusters on screen at the level the map draws, each fitting the map
 * to it when pressed). While a region is selected its card goes first, and Colour and In view fold
 * to one-line headings with a hint of what they hold; pressing one opens it, the chevron beside an
 * opened one folds it again, and clearing the region unfolds them. The hint names the colouring and
 * how many values the current set holds, counted by the aggregate route once it answers, or how
 * many clusters are in view. The status strip sits in the map's bottom-right.
 *
 * Picking a point opens its item card beside the point, joined to it by a short leader: on the side
 * with most room, clear of the left and right cards, following the point as the map pans and zooms,
 * hidden while the point is off the map, and clear of the other cards beside points. The card shows
 * the headline, the `subtitle-field` field, three fields and "Show all N fields". Picking another
 * point replaces it unless Pin was pressed, which keeps it open; Close or Escape closes it and drops
 * the selection, and a click on the map that finds nothing does the same for the card that is not
 * pinned. Tab from the map goes to the card, which is named by its item's title and gives focus back
 * to the map as it closes. A picked cluster's card behaves the
 * same way. Where the map holds no position for the selection, as after following an item into
 * another view, the card goes first in the right column, as a region's does.
 *
 * The Layers button opens a popover with a Display section (whether the points are drawn, what sizes
 * them, their opacity, and how density is drawn, at what resolution, in which colours and how
 * strongly) over the layer picker. Size by lists None and the number columns the points arrive with. Under None one Size
 * slider sets every point's radius, which `hide-size` leaves out; under a column two sliders set the
 * radii of its smallest and largest value, and a Linear, Log or Rank choice places values between
 * them. The display settings are the explorer's properties of the same names, passed to its map.
 * `pinned-filters` names the columns whose controls are listed before they hold a clause.
 *
 * In a container narrower than 1000 px, either layout takes the overlay's form with both cards
 * narrower: the left 300 px, the right 272 px with Colour and In view folded until opened, and a
 * legend naming four values with the rest under "N more"; the tools move to the bottom-left and the
 * strip shortens its figures. In a container 720 px wide or narrower, the status strip runs full
 * width above a tab bar (Filters, Layers, In view, Item), and each tab opens its panel as a sheet;
 * the item card is the Item sheet there, and the cards over the map are not drawn.
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
 * @slot artifacts - Replaces the In view list.
 * @slot selection - Replaces the selection panel, shown while a region is selected.
 * @slot detail - Replaces the item and artifact cards, in the card beside the point.
 * @slot status - Replaces the status strip drawn on the map.
 * @slot tooltip - Replaces the map's hover tooltip.
 * @slot top-right - Content in the map's top-right corner, above the right-hand cards.
 * @fires {CustomEvent<TesseraEventDetails['tessera-viewchange']>} tessera-viewchange - The map's camera moved.
 * @fires {CustomEvent<TesseraEventDetails['tessera-pick']>} tessera-pick - A point was clicked, and again with its record.
 * @fires {CustomEvent<TesseraEventDetails['tessera-miss']>} tessera-miss - A click on the map found nothing; the explorer closes the card that is not pinned and drops the selection.
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
 * @fires {CustomEvent<TesseraEventDetails['tessera-artifactfit']>} tessera-artifactfit - Fit was pressed on the artifact card, or a row of In view; the explorer fits its map to the artifact.
 * @fires {CustomEvent<TesseraEventDetails['tessera-open']>} tessera-open - Open was pressed on the item card.
 * @fires {CustomEvent<TesseraEventDetails['tessera-close']>} tessera-close - A card's close button was pressed; the explorer closes the card and drops the selection it shows.
 * @fires {CustomEvent<TesseraEventDetails['tessera-viewswitch']>} tessera-viewswitch - The view changed through the view or key picker.
 * @fires {CustomEvent<TesseraEventDetails['tessera-viewfollow']>} tessera-viewfollow - A view chip on the item card was pressed; the explorer switches to that view and centres on the item.
 * @csspart frame - The explorer's grid.
 * @csspart sidebar - The sidebar, in the docked layout.
 * @csspart panel - The card over the map's top-left, in the overlay layout and in a container
 *   narrower than 1000 px.
 * @csspart dataset-title - The heading's dataset title, where `dataset-title` is set.
 * @csspart view-name - The view's name in the heading, where there is one view to show.
 * @csspart selection-card - The card in the map's top-right corner holding the selection, while a
 *   region is selected.
 * @csspart info - The card in the map's top-right corner holding Colour and In view.
 * @csspart fold - A folded section's one-line heading in that card, with `data-section` (`colour`
 *   or `in-view`) and `aria-expanded`; pressing it opens the section.
 * @csspart fold-hint - What a folded section holds, in its heading.
 * @csspart callout - A card beside a point on the map, with `data-side` (`right`, `left`, `below`
 *   or `above`) and `data-pinned` while pinned.
 * @csspart pin - A callout's Pin button, with `aria-pressed`.
 * @csspart leaders - The lines joining each callout to its point.
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
 * @csspart density-resolution - The Resolution slider, while density is drawn: the cell size on
 *   screen, coarse to fine. Its stops past the finest the server will count for the view are shown
 *   struck through, and the slider stops at the last one it can count.
 * @csspart density-scale - The Linear and Log choice of density's colour scale, while density is
 *   drawn, each with `data-scale` and `aria-checked`.
 * @csspart density-colours - The button that opens the list of density colours, while density is
 *   drawn in colours.
 * @csspart density-strength - The Strength slider, while density is drawn.
 * @csspart detail - The item or artifact card in the map's top-right corner, where the map holds no
 *   position for the selection.
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
 * @csspart cluster-filter-<part> - A part of a `<tessera-cluster-filter>` inside the filter panel.
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
        --_right-width: 320px;
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
        --_right-width: 272px;
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
      /* What the map draws at its top-left, its tools or a region's tag, sits right of the card; in
         the narrow layout there is no card. */
      @container explorer (width > 720px) {
        .floating tessera-map {
          --tessera-map-inset-left: calc(var(--_panel-width) + var(--_tessera-space));
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
      [part='panel'] tessera-filter-panel,
      [part='sidebar'] tessera-filter-panel {
        --_tessera-panel-inline: 14px;
      }
      .compact [part='panel'] {
        --_tessera-panel-padding: 10px 12px;
      }
      /* Room beneath for the tools and the Layers button in the bottom-left corner. */
      .compact [part='panel'] {
        max-height: calc(100% - 2 * var(--_tessera-space) - 200px);
      }
      /* A typeahead in the card opens over what sits below it, and the card scrolls to show it. */
      [part='panel'] tessera-filter-panel,
      [part='sidebar'] tessera-filter-panel {
        position: relative;
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
      [part='selection-card'],
      [part='info'] {
        --_tessera-panel-padding: 12px 14px;
        width: var(--_right-width);
        max-width: 100%;
        min-height: 0;
        overflow-y: auto;
      }
      [part='detail'],
      [part='info'] {
        flex: 0 1 auto;
      }
      [part='info'] tessera-legend,
      [part='info'] tessera-artifact-list {
        display: block;
      }
      [part='info'] tessera-artifact-list {
        --_tessera-panel-padding: 12px 8px 10px;
      }
      [part='info'] tessera-artifact-list::part(title) {
        padding: 0 6px;
      }
      [part='info'] > :last-child {
        --_tessera-panel-rule: transparent;
      }
      [part='fold'] {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 10px;
        width: 100%;
        padding: 10px 14px;
        text-align: left;
        font-size: 11px;
        font-weight: 600;
        letter-spacing: 0.02em;
        text-transform: uppercase;
        color: var(--_tessera-ink-2);
      }
      [part='fold'] + [part='fold'] {
        border-top: 1px solid var(--_tessera-line-2);
      }
      [part='fold']:hover {
        background: var(--_tessera-surface-2);
      }
      /* An opened section keeps a chevron in its heading's gutter that folds it again. */
      .section {
        position: relative;
      }
      .section + .section,
      [part='fold'] + .section,
      .section + [part='fold'] {
        border-top: 1px solid var(--_tessera-line-2);
      }
      .section > .refold {
        position: absolute;
        z-index: 1;
        top: 11px;
        left: 6px;
        width: 16px;
        height: 16px;
        padding: 0;
        display: grid;
        place-items: center;
        border-radius: 4px;
      }
      .section tessera-legend::part(title),
      .section tessera-artifact-list::part(title) {
        padding-left: 12px;
      }
      [part='fold-hint'] {
        min-width: 0;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
        text-transform: none;
        letter-spacing: 0;
        font-size: 12px;
        font-weight: 400;
        color: var(--_tessera-ink-3);
      }
      /* The cards beside points on the map, and the lines joining them to their points. */
      .callouts {
        position: absolute;
        inset: 0;
        z-index: 3;
        pointer-events: none;
        overflow: hidden;
      }
      [part='leaders'] {
        position: absolute;
        inset: 0;
        width: 100%;
        height: 100%;
      }
      [part='leaders'] line {
        stroke: var(--_tessera-ink);
        stroke-width: 1.5;
      }
      [part='leaders'] circle {
        fill: var(--_tessera-surface);
        stroke: var(--_tessera-ink);
        stroke-width: 1.5;
      }
      [part~='callout'] {
        position: absolute;
        left: 0;
        top: 0;
        width: 300px;
        pointer-events: auto;
        box-shadow: 0 8px 28px rgba(0, 0, 0, 0.14);
        --_tessera-panel-padding: 12px 14px 10px;
        --_tessera-panel-inline: 14px;
        --_tessera-panel-rule: transparent;
        --_tessera-key-width: 104px;
        --_tessera-field-gap: 4px 10px;
        font-size: 12px;
        overflow-y: auto;
        user-select: none;
      }
      /* A callout whose point is off the map keeps its box, so its size stays known. */
      [part~='callout'][hidden] {
        display: block;
        visibility: hidden;
      }
      [part='pin'] {
        flex: none;
        width: 24px;
        height: 24px;
        margin: -2px 0 0;
        display: grid;
        place-items: center;
        border-radius: 5px;
        color: var(--_tessera-ink-2);
      }
      [part='pin']:hover {
        background: var(--_tessera-surface-2);
      }
      [part='pin'][aria-pressed='true'] {
        background: var(--_tessera-surface-3);
        color: var(--_tessera-ink);
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
      .compact [part='detail'] tessera-item-card,
      .compact [part='detail'] tessera-artifact-card {
        font-size: 12px;
      }
      .compact [part='detail'] {
        --_tessera-panel-padding: 12px 12px 10px;
        --_tessera-title-size: 14px;
        --_tessera-key-width: 104px;
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
        .callouts,
        .in-map-strip {
          display: none;
        }
        [part='tabs'] {
          display: flex;
          height: 56px;
          padding: 0 8px env(safe-area-inset-bottom, 0px);
          border-top: 1px solid var(--_tessera-line);
          background: var(--_tessera-surface);
        }
        [part='tabs'] button {
          flex: 1 1 0;
          min-width: 0;
          display: flex;
          flex-direction: column;
          align-items: center;
          justify-content: center;
          gap: 4px;
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
        /* Every heading and row in a sheet starts 16 px in: the In view rows' fill reaches past
           their content, so the list is drawn out by it. */
        [part='sheet'] tessera-artifact-list::part(items) {
          margin: 0 -6px;
        }
        [part='sheet'] tessera-artifact-list::part(more) {
          margin-left: 0;
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
   * Which regions appear, space- or comma-separated, from `toolbar` (the heading and the
   * legend), `legend` (the Layers button and its layer picker), `filters`, `artifacts` (In view),
   * `selection` and `detail` (the item and artifact cards). Defaults to all six. Any other name,
   * such as `hierarchy`, shows nothing.
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
  /** The field the item card shows under its headline. Unset, it shows none. */
  @property({attribute: 'subtitle-field'}) accessor subtitleField = '';
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
  /**
   * Passed to the map's `density-resolution`, the cell size on screen in CSS pixels; the Resolution
   * slider in the Layers popover changes it.
   */
  @property({type: Number, attribute: 'density-resolution'}) accessor densityResolution = DEFAULT_DENSITY_CELL_PX;
  /** Passed to the map's `density-scale`; the Scale choice under Resolution in the Layers popover changes it. */
  @property({attribute: 'density-scale'}) accessor densityScale: DensityScale = DEFAULT_DENSITY_SCALE;
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
  /** The cards kept open by Pin, oldest first. @internal */
  @state() accessor pinned: Pinned[] = [];
  /** The sections of the right card opened while they fold by default. */
  private unfolded = new Set<'colour' | 'in-view'>();
  /** Whether the right card's sections folded by default at the last render, `null` before it. */
  private foldedWas: boolean | null = null;
  /** Each callout's size as last measured, by its key. */
  private calloutSizes = new Map<string, {width: number; height: number}>();
  /** The cards a callout keeps clear of, in the map's pixels, as last measured. */
  private keepClear: Rect[] = [];
  /** Whether the Layers popover is open. @internal */
  @state() accessor layersOpen = false;
  /** Whether the container is narrower than 1000 px and wider than the narrow layout. @internal */
  @state() accessor compact = false;
  /** Whether the container is 720 px wide or narrower, where the panels are sheets. @internal */
  @state() accessor narrow = false;
  /** The cards beside points as last rendered, which {@link placeCallouts} places. */
  private cards: {key: string; world: [number, number]; pinned: boolean}[] = [];
  /** The map's size as last measured; read from the map where nothing has measured it. */
  private mapSize: {width: number; height: number} | null = null;
  /** Measures the map, the cards over it and the callouts as they change size. */
  private sizes: ResizeObserver | null = null;
  private observed = new Set<Element>();
  /** A frame asked for to place the callouts after the camera moved. */
  private placing: number | null = null;
  /** Each callout's last placement, by its key, with the point it was placed for. */
  private placements = new Map<string, {side: Side; offset: number; world: [number, number]}>();
  /** Set while the camera moves, and cleared {@link CALLOUT_REST_MS} after it stops. */
  private moving: ReturnType<typeof setTimeout> | null = null;
  /** The zoom the level drawn was last worked out at, and the level that gave when none is chosen. */
  private zoomSeen: number | null = null;
  private autoSeen: number | null = null;
  /** The count of values behind the folded Colour heading. */
  private readonly hintCount = new HeldAggregate('colour-hint');
  /** The folded In view heading, kept while nothing it counts has changed. */
  private inViewHeld: {key: string; hint: string} | null = null;

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

  /** Stops following the size choices of the store adopted last, which the Display section shows. */
  private unwatchChoices: (() => void) | null = null;

  /** The pinned cards and what the callouts measured name records the server answered. */
  protected override resetServerData(): void {
    this.pinned = [];
    this.calloutSizes.clear();
    const m = this.map;
    if (m) m.pickedAt = null;
  }

  protected override onStoreAdopted(store: Store | null): void {
    this.provider.setValue(store);
    this.unwatchChoices?.();
    this.unwatchChoices = store ? watchChoices(store, () => this.requestUpdate()) : null;
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
    // The width is taken once now, so the first render already has the layout it will keep.
    this.fitWidth(this.getBoundingClientRect().width);
    this.resize ??= new ResizeObserver((entries) => this.fitWidth(entries.at(-1)?.contentRect.width ?? 0));
    this.resize.observe(this);
    this.sizes ??= new ResizeObserver(() => this.measure());
  }

  /** Take the compact or the narrow form, or neither, for a container `width` pixels wide. */
  private fitWidth(width: number): void {
    this.compact = width > COMPACT_BETWEEN[0] && width < COMPACT_BETWEEN[1];
    this.narrow = width > 0 && width <= COMPACT_BETWEEN[0];
  }

  override disconnectedCallback(): void {
    // A follow in flight holds its own subscription, which the base class does not drop.
    this.following?.();
    this.resize?.disconnect();
    this.resize = null;
    this.sizes?.disconnect();
    this.sizes = null;
    this.observed.clear();
    if (this.placing !== null) cancelAnimationFrame(this.placing);
    this.placing = null;
    if (this.moving !== null) clearTimeout(this.moving);
    this.moving = null;
    this.hintCount.set(null, null);
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
    const artifacts = s?.get('artifacts');
    const layersOn = artifacts?.layers.length ?? 0;
    const compact = this.compact;
    // The narrow layout shows its panels as sheets and draws none of the cards over the map.
    const narrow = this.narrow;
    // Short of room, the docked layout takes the overlay's form.
    const floating = this.layout === 'overlay' || compact;
    // The level drawn: the one chosen through the legend, else, for a levelled layer served
    // whole, the level the view's budget would cut at, else the deepest served.
    const autoLevel = this.autoLevel();
    this.zoomSeen = this.map?.zoom ?? null;
    this.autoSeen = autoLevel;
    const level = this.level ?? autoLevel;
    // A click that found nothing shows no card; a broken pick shows its fault.
    const hasDetail = Boolean(selection?.item || selection?.artifact || selection?.artifactRefusal || selection?.itemRefusal || this.map?.lastPick?.kind === 'broken');
    // The detail shows whichever changed last.
    const showArtifact = this.showsArtifact();
    const detail = this.liveCard(showArtifact);
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
    const filters = html`<slot name="filters"><tessera-filter-panel exportparts=${FORWARD['filter-panel']} pinned=${this.pinnedFilters || nothing}></tessera-filter-panel></slot>`;
    const list = html`<slot name="artifacts"><tessera-artifact-list exportparts=${FORWARD['artifact-list']} .level=${level} .rows=${8}></tessera-artifact-list></slot>`;
    const selectionPanel = this.has('selection') && region ? html`<slot name="selection"><tessera-selection exportparts=${FORWARD.selection}></tessera-selection></slot>` : nothing;
    const selectionCard = selectionPanel === nothing ? nothing : html`<div part="selection-card" class="card">${selectionPanel}</div>`;
    const head = html`<div class="head" ?hidden=${!named}>${names}</div>`;

    // The card beside the point, where the map holds a position for what is selected; else the
    // card goes first in the right column.
    const anchor = this.liveAnchor(showArtifact);
    const detailInColumn = this.has('detail') && hasDetail && anchor === null;

    // The right card: Colour, then In view. They fold to one line each while a region or a card
    // takes the column's top, and in the compact form, until opened.
    const foldedByDefault = compact || selectionCard !== nothing || detailInColumn;
    if (foldedByDefault !== this.foldedWas) {
      this.foldedWas = foldedByDefault;
      this.unfolded = new Set();
    }
    const folded = (section: 'colour' | 'in-view') => foldedByDefault && !this.unfolded.has(section);
    const fold = (section: 'colour' | 'in-view', title: string, hint: string) =>
      html`<button part="fold" type="button" data-section=${section} aria-expanded="false"
        @click=${() => {
          this.unfolded = new Set([...this.unfolded, section]);
          this.requestUpdate();
        }}><span>${title}</span><span part="fold-hint">${hint}</span></button>`;
    // An opened section, while the sections fold by default, keeps a chevron that folds it again.
    const opened = (section: 'colour' | 'in-view', title: string, body: unknown) =>
      foldedByDefault
        ? html`<div class="section" data-section=${section}><button part="fold" class="refold" type="button" data-section=${section} aria-expanded="true" aria-label=${`Fold ${title}`}
            @click=${() => {
              const next = new Set(this.unfolded);
              next.delete(section);
              this.unfolded = next;
              this.requestUpdate();
            }}>${icon('chev', 12, 1.4)}</button>${body}</div>`
        : body;
    const sections = [
      this.has('toolbar') ? (folded('colour') ? fold('colour', 'Colour', this.colourHint()) : opened('colour', 'Colour', colour)) : nothing,
      this.has('artifacts') ? (folded('in-view') ? fold('in-view', 'In view', this.inViewHint(level)) : opened('in-view', 'In view', list)) : nothing
    ];
    this.hintCount.set(s, this.isConnected && !narrow && this.has('toolbar') && folded('colour') ? this.hintSpec() : null);
    const info = this.has('toolbar') || this.has('artifacts') ? html`<div part="info" class="card">${sections}</div>` : nothing;
    const right = html`<div slot="top-right" class="right"><slot name="top-right"></slot>${selectionCard}${detailInColumn ? html`<div part="detail" class="card">${detail}</div>` : nothing}${info}</div>`;

    const docked = html`<aside part="sidebar" aria-label="Explorer panels">
      ${this.has('toolbar') ? head : nothing}
      ${this.has('filters') ? filters : nothing}
    </aside>`;
    const panel =
      this.has('toolbar') || this.has('filters')
        ? html`<div part="panel" class="card" role="region" aria-label="Explorer panels">${this.has('toolbar') ? head : nothing}${this.has('filters') ? filters : nothing}</div>`
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
    const counted = s?.get('view').inView;
    const matchedText = counted && counted.status === 'shown' && counted.matched.exact ? `Show ${counted.matched.value.toLocaleString('en-GB')} matching` : 'Show';
    // Clear has nothing to do where no filter, highlight or cluster clause applies.
    const applied = s ? activeCount(s.get('filters').draft) > 0 || s.get('filters').members.length > 0 : false;
    const sheetFooter = html`<div class="sheet-footer">
      <button class="btn" type="button" ?disabled=${!applied} @click=${() => {
        const meta = s?.get('meta');
        if (!s || !meta) return;
        s.setFilters(emptyDraft(meta.filterOperands));
        s.setMembers([]);
      }}>Clear</button>
      <button class="btn primary" type="button" @click=${() => (this.sheet = null)}>${matchedText}</button>
    </div>`;
    const sheetBody =
      this.sheet === 'filters'
        ? html`${filters}${sheetFooter}`
        : this.sheet === 'layers'
          ? html`${colour}${layersPanel}`
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
      @tessera-close=${(e: Event) => this.closeDetail(e)}>
      ${floating || narrow ? nothing : docked}
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
          .densityResolution=${this.densityResolution}
          .densityScale=${this.densityScale}
          .categoryPalette=${this.categoryPalette}
          .ramp=${this.ramp}
          .rampScale=${this.rampScale}
          .rampReverse=${this.rampReverse}
          .valueColours=${this.valueColours}
          @tessera-viewchange=${() => this.onCamera()}
          @tessera-pick=${() => this.requestUpdate()}
          @tessera-miss=${() => this.onMiss()}
          @keydown=${this.onMapKey}
          @click=${() => this.requestUpdate()}
        >
          ${narrow ? nothing : layersButton}
          ${narrow ? nothing : right}
          ${narrow ? nothing : html`<div slot="bottom-right" class="in-map-strip"><slot name="status"><tessera-status exportparts=${FORWARD.status} ?compact=${compact}></tessera-status></slot></div>`}
          ${this.querySelector('[slot="tooltip"]') ? html`<slot name="tooltip" slot="tooltip"></slot>` : nothing}
        </tessera-map>
        ${narrow ? nothing : this.callouts(this.has('detail') && hasDetail ? anchor : null, detail)}
        ${floating && !narrow ? panel : nothing}
      </div>
      ${this.sheet && sheetBody !== nothing
        ? html`<div part="sheet" id="sheet" role="dialog" aria-labelledby=${`tab-${this.sheet}`} tabindex="-1" @keydown=${this.onSheetKey}>${sheetBody}</div>`
        : nothing}
      <div part="strip-row"><tessera-status exportparts=${FORWARD.status}></tessera-status></div>
      <div part="tabs" role="tablist" aria-label="Explorer panels" @keydown=${this.onTabKey}>${tabs.map(tab)}</div>
    </div>`;
  }

  /** The item or artifact card for what is selected, or the host's in the `detail` slot. */
  private liveCard(showArtifact: boolean, pin: TemplateResult | typeof nothing = nothing): TemplateResult {
    return html`<slot name="detail">${showArtifact
      ? html`<tessera-artifact-card exportparts=${FORWARD['artifact-card']}>${pin}</tessera-artifact-card>`
      : html`<tessera-item-card exportparts=${FORWARD['item-card']} compact title-field=${this.titleField || nothing} subtitle-field=${this.subtitleField || nothing} .pick=${this.map?.lastPick ?? null}>${pin}</tessera-item-card>`}</slot>`;
  }

  /** Whether the card for the selection is the artifact card: the artifact, or its refusal, changed last. */
  private showsArtifact(): boolean {
    const sel = this.resolvedStore?.get('selection');
    return this.lastDetail === 'artifact' && Boolean(sel?.artifact || sel?.artifactRefusal);
  }

  /**
   * The key the selection's card goes by, `item:<id>` or `artifact:<id>`, or `null` where the card
   * shows a refusal or nothing, which cannot be pinned.
   */
  private liveKey(): string | null {
    const sel = this.resolvedStore?.get('selection');
    if (this.showsArtifact()) return sel?.artifact ? `artifact:${sel.artifact.id}` : null;
    return sel?.item ? `item:${sel.item.id}` : null;
  }

  /** What a card is called: its item's title field, else its `tessera_id`; a cluster's name. */
  private cardName(key: string | null): string {
    const sel = this.resolvedStore?.get('selection');
    const pinned = this.pinned.find((p) => p.key === key);
    const item = pinned?.kind === 'item' ? pinned.item : key?.startsWith('item:') ? sel?.item : null;
    if (item) {
      const title = this.titleField ? item.detail.fields[this.titleField] : undefined;
      return title === undefined || title === null || title === '' ? `Item ${item.id}` : String(title);
    }
    const id = pinned?.kind === 'artifact' ? pinned.artifact.id : key?.startsWith('artifact:') ? sel?.artifact?.id : undefined;
    if (id !== undefined) {
      const a = this.resolvedStore?.get('artifacts');
      const row = a?.served.find((x) => x.tesseraId === id) ?? a?.colourServed.find((x) => x.tesseraId === id);
      return (row && a ? artifactName(row, a.attached) : null) ?? 'Cluster';
    }
    return 'Unavailable';
  }

  /**
   * Where the selection's card points, in world coordinates: where the map picked it, while the map
   * holds that pick for what is selected. `null` otherwise.
   */
  private liveAnchor(showArtifact: boolean): [number, number] | null {
    const at = this.map?.pickedAt;
    const sel = this.resolvedStore?.get('selection');
    if (!at || !sel) return null;
    if (showArtifact) return at.kind === 'artifact' && (sel.artifact === null || sel.artifact.id === at.id) ? at.world : null;
    if (at.kind !== 'item') return null;
    return sel.item === null || sel.item.id === at.id ? at.world : null;
  }

  /** Keep the selection's card open when another point is picked, or let it go again. */
  private togglePin(): void {
    const key = this.liveKey();
    const sel = this.resolvedStore?.get('selection');
    const at = this.map?.pickedAt;
    const view = this.resolvedStore?.get('view').id ?? '';
    if (!key || !sel || !at) return;
    if (this.pinned.some((p) => p.key === key)) {
      this.pinned = this.pinned.filter((p) => p.key !== key);
      return;
    }
    if (key.startsWith('artifact:') && sel.artifact) this.pinned = [...this.pinned, {key, kind: 'artifact', artifact: sel.artifact, world: at.world, view}];
    else if (sel.item) this.pinned = [...this.pinned, {key, kind: 'item', item: sel.item, world: at.world, view}];
  }

  /**
   * The cards beside their points: each pinned card, then the selection's where `anchor` places it
   * and no pinned card shows it already. {@link placeCallouts} places them, and hides a card whose
   * point is off the map.
   */
  private callouts(anchor: [number, number] | null, detail: TemplateResult): TemplateResult | typeof nothing {
    const m = this.map;
    const view = this.resolvedStore?.get('view').id ?? '';
    if (!m) return nothing;
    const live = this.liveKey();
    const pinButton = (pressed: boolean, onPress: () => void) =>
      html`<button part="pin" slot="actions" type="button" aria-pressed=${pressed ? 'true' : 'false'} aria-label=${pressed ? 'Unpin' : 'Pin'} title=${pressed ? 'Unpin' : 'Keep open'} @click=${onPress}>${icon('pin', 13, 1.5)}</button>`;
    const cards: {key: string; world: [number, number]; body: TemplateResult; pinned: boolean; name: string}[] = this.pinned
      .filter((p) => p.view === view)
      .map((p) => {
        const unpin = pinButton(true, () => (this.pinned = this.pinned.filter((q) => q.key !== p.key)));
        const body =
          p.kind === 'item'
            ? html`<tessera-item-card exportparts=${FORWARD['item-card']} compact .item=${p.item} title-field=${this.titleField || nothing} subtitle-field=${this.subtitleField || nothing}>${unpin}</tessera-item-card>`
            : html`<tessera-artifact-card exportparts=${FORWARD['artifact-card']} .artifact=${p.artifact}>${unpin}</tessera-artifact-card>`;
        return {key: p.key, world: p.world, body, pinned: true, name: this.cardName(p.key)};
      });
    if (anchor && !(live && this.pinned.some((p) => p.key === live && p.view === view))) {
      const body = live ? this.liveCard(this.showsArtifact(), pinButton(false, () => this.togglePin())) : detail;
      cards.push({key: 'live', world: anchor, body, pinned: false, name: this.cardName(live)});
    }
    this.cards = cards.map(({key, world, pinned}) => ({key, world, pinned}));
    if (cards.length === 0) return nothing;
    return html`<div class="callouts">
      <svg part="leaders" aria-hidden="true">
        ${repeat(cards, (c) => c.key, (c) => svg`<g data-callout=${c.key}><line></line>${c.pinned ? svg`<circle r="4"></circle>` : nothing}</g>`)}
      </svg>
      ${repeat(
        cards,
        (c) => c.key,
        (c) => html`<div part="callout" class="card" role="dialog" tabindex="-1" aria-label=${c.name} data-callout=${c.key} ?data-pinned=${c.pinned} @keydown=${this.onCalloutKey}>${c.body}</div>`
      )}
    </div>`;
  }

  /**
   * Place each callout beside its point under the camera as it stands, clear of the cards over the
   * map and of the callouts placed before it, and join it to its point. A callout whose point is off
   * the map is hidden with its leader. Reads no layout: the sizes are those {@link measure} took.
   */
  private placeCallouts(firm = this.moving !== null): void {
    const m = this.map;
    if (!m) return;
    const size = this.mapSize ?? {width: m.clientWidth, height: m.clientHeight};
    const placed: Rect[] = [];
    const keys = new Set<string>();
    for (const el of Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="callout"]'))) {
      const key = el.dataset.callout!;
      const card = this.cards.find((c) => c.key === key);
      const leader = this.renderRoot.querySelector<SVGGElement>(`[part="leaders"] g[data-callout="${key}"]`);
      if (!card) continue;
      keys.add(key);
      const point = m.screenOf(card.world);
      const box = this.calloutSizes.get(key) ?? CALLOUT_SIZE;
      // A card keeps its side and its place along it while the camera moves and as it grows, and
      // at rest wherever that is still clear; a card for another point is placed afresh.
      const last = this.placements.get(key);
      const kept = last && last.world[0] === card.world[0] && last.world[1] === card.world[1] ? {side: last.side, offset: last.offset, firm} : null;
      const at = size.width > 0 && size.height > 0 ? placeCallout(point, box, size, [...this.keepClear, ...placed], undefined, kept, CALLOUT_MIN_HEIGHT) : null;
      el.hidden = at === null;
      leader?.setAttribute('visibility', at === null ? 'hidden' : 'visible');
      if (!at) continue;
      // A card shortened to fit scrolls inside, so its last row stays within reach.
      el.style.maxHeight = `${Math.round(at.height)}px`;
      this.placements.set(key, {side: at.side, offset: at.offset, world: card.world});
      placed.push({left: at.left, top: at.top, width: box.width, height: at.height});
      el.style.transform = `translate(${Math.round(at.left)}px, ${Math.round(at.top)}px)`;
      el.dataset.side = at.side;
      const line = leader?.querySelector('line');
      line?.setAttribute('x1', String(at.leader.x1));
      line?.setAttribute('y1', String(at.leader.y1));
      line?.setAttribute('x2', String(at.leader.x2));
      line?.setAttribute('y2', String(at.leader.y2));
      const dot = leader?.querySelector('circle');
      dot?.setAttribute('cx', String(point[0]));
      dot?.setAttribute('cy', String(point[1]));
    }
    for (const key of [...this.placements.keys()]) if (!keys.has(key)) this.placements.delete(key);
  }

  /**
   * Take the map's size, each callout's size and where the cards callouts keep clear of are, then
   * place the callouts. Run as any of them changes size, not as the camera moves.
   */
  private measure(): void {
    const m = this.map;
    if (!m) return;
    this.mapSize = {width: m.clientWidth, height: m.clientHeight};
    const origin = m.getBoundingClientRect();
    // The height the card's content wants, whatever height it is shown at.
    for (const el of Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="callout"]'))) {
      if (el.offsetWidth > 0) this.calloutSizes.set(el.dataset.callout!, {width: el.offsetWidth, height: Math.max(el.offsetHeight, el.scrollHeight)});
    }
    this.keepClear = this.keptClear().flatMap((el) => {
      const r = el.getBoundingClientRect();
      return r.width > 0 && r.height > 0 ? [{left: r.left - origin.left, top: r.top - origin.top, width: r.width, height: r.height}] : [];
    });
    // A card that grew, as under Show all, keeps its side and grows down where that is clear.
    this.placeCallouts();
  }

  /** The cards over the map that callouts keep clear of. */
  private keptClear(): Element[] {
    const q = (sel: string) => Array.from(this.renderRoot.querySelectorAll(sel));
    return [...q('[part="panel"]'), ...q('.right > *'), ...q('.in-map-strip'), ...q('.layers .group')];
  }

  /**
   * After a render: follow the size of the map, the cards over it and the callouts, forget the size
   * of a callout that has closed, and place the callouts. Where nothing reports sizes, measure now.
   */
  private followLayout(): void {
    const m = this.map;
    const callouts = Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="callout"]'));
    const keys = new Set(callouts.map((el) => el.dataset.callout!));
    for (const key of [...this.calloutSizes.keys()]) if (!keys.has(key)) this.calloutSizes.delete(key);
    if (!this.sizes) {
      if (callouts.length > 0) this.measure();
      return;
    }
    const now = new Set<Element>([...(m ? [m] : []), ...this.keptClear(), ...callouts]);
    for (const el of this.observed) if (!now.has(el)) this.sizes.unobserve(el);
    for (const el of now) if (!this.observed.has(el)) this.sizes.observe(el);
    this.observed = now;
    this.placeCallouts();
  }

  /**
   * The camera moved: place the callouts in the next frame, and draw again only where the zoom
   * changed the level the layers are drawn at.
   */
  /** The enabled resolution stops the popover last drew, so a camera change that alters them redraws it. */
  private stopsSeen = '';

  private onCamera(): void {
    if (this.moving !== null) clearTimeout(this.moving);
    this.moving = setTimeout(() => {
      this.moving = null;
      this.placeCallouts();
    }, CALLOUT_REST_MS);
    if (this.placing !== null || typeof requestAnimationFrame === 'undefined') return;
    this.placing = requestAnimationFrame(() => {
      this.placing = null;
      this.placeCallouts();
      // The resolution stops the server can count change with the camera.
      // The stops and the readout's cell size change with the zoom.
      const stops = this.layersOpen && this.density !== 'none' ? `${Math.round((this.map?.zoom ?? 0) * 8)}:${this.map?.densityStops().map((s) => `${s.depth}${s.enabled ? '+' : '-'}`).join('') ?? ''}` : '';
      if (stops !== this.stopsSeen) {
        this.stopsSeen = stops;
        this.requestUpdate();
      }
      const zoom = this.map?.zoom ?? null;
      if (zoom !== this.zoomSeen && this.level === null) {
        this.zoomSeen = zoom;
        if (this.autoLevel() !== this.autoSeen) this.requestUpdate();
      }
    });
  }

  /**
   * Tab from the map itself goes to the card beside the point, the selection's where it shows, so
   * the card is the next stop after the map; from there Tab goes on in the page's order.
   */
  private onMapKey = (e: KeyboardEvent): void => {
    if (e.key !== 'Tab' || e.shiftKey || e.composedPath()[0] !== this.map) return;
    const shown = Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="callout"]')).filter((c) => !c.hidden);
    const card = shown.find((c) => c.dataset.callout === 'live') ?? shown[0];
    if (!card) return;
    e.preventDefault();
    card.focus();
  };

  /** Escape on a card: the card not pinned closes, dropping the selection; focus goes back to the map. */
  private onCalloutKey = (e: KeyboardEvent): void => {
    if (e.key !== 'Escape') return;
    e.stopPropagation();
    const key = (e.currentTarget as HTMLElement).dataset.callout;
    if (key === 'live') this.closeDetail(e);
    else this.map?.focus();
  };

  /**
   * The aggregate behind the folded Colour heading under a category colouring: how many values the
   * current set holds. `null` under any other colouring.
   */
  private hintSpec(): AggregateSpec | null {
    const s = this.resolvedStore;
    const colourBy = s?.get('legend').colourBy ?? null;
    const meta = s?.get('meta');
    if (!colourBy || !meta?.declaredScalars.some((c) => c.name === colourBy && c.category && c.render)) return null;
    return {groupings: [{by: {field: colourBy, top: 1}}]};
  }

  /**
   * What the folded Colour heading says: what the points are coloured by, then how many values the
   * current set holds once the aggregate answers, or how many of the layer's clusters are in view.
   */
  private colourHint(): string {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    const legend = s?.get('legend');
    const colourBy = legend?.colourBy ?? null;
    if (!s || !meta || !legend || colourBy === null) return 'None';
    const layer = clusterLayerOf(colourBy);
    if (layer) {
      const n = s.get('artifacts').colourServed.filter((a) => a.layer === layer).length;
      const decl = meta.layers.find((l) => l.name === layer);
      return `${decl?.title || layer} · ${n.toLocaleString('en-GB')} cluster${n === 1 ? '' : 's'}`;
    }
    const n = this.hintCount.entry()?.result?.tables[0]?.groups ?? null;
    return n === null ? columnCaption(colourBy) : `${columnCaption(colourBy)} · ${n.toLocaleString('en-GB')} value${n === 1 ? '' : 's'}`;
  }

  /** What the folded In view heading says: how many clusters are on screen at the level drawn. */
  private inViewHint(level: number | null): string {
    const s = this.resolvedStore;
    const a = s?.get('artifacts');
    if (!s || !a) return '';
    const colourBy = s.get('legend').colourBy;
    const key = `${a.version}|${level}|${colourBy}`;
    if (this.inViewHeld?.key === key) return this.inViewHeld.hint;
    const colourLayer = clusterLayerOf(colourBy);
    const source = colourLayer ? a.colourServed.filter((x) => x.layer === colourLayer) : a.served;
    const n = listedAt(source, level, s.get('meta'), colourLayer === null).length;
    const hint = `${n.toLocaleString('en-GB')} cluster${n === 1 ? '' : 's'}`;
    this.inViewHeld = {key, hint};
    return hint;
  }

  /** The display settings as they stand. */
  private get display(): DisplaySettings {
    return {
      points: !this.noPoints,
      radius: this.radius,
      pointOpacity: this.pointOpacity,
      density: this.density,
      densityColours: this.densityColours || null,
      densityStrength: this.densityStrength,
      densityResolution: this.densityResolution,
      densityScale: drawnDensityScale(this.densityScale)
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
            ${this.resolutionControl(change)}
            ${this.densityScaleControl(s.densityScale, change)}
            ${ramped
              ? html`<span id="colours-label">Colours</span>
                  <button part="density-colours" class="ramp-choice" type="button" aria-labelledby="colours-label" aria-expanded=${this.densityColoursOpen ? 'true' : 'false'} aria-controls="density-colour-list"
                    @click=${() => (this.densityColoursOpen = !this.densityColoursOpen)}><span class="bar" style=${`background:${densityGradient(colours, scheme)}`}></span>${DENSITY_COLOUR_TITLES[colours]}${icon('chev', 12, 1.4)}</button>
                  ${colourList}`
              : nothing}
            <label for="density-strength">Strength</label>
            <div class="with-readout"><input id="density-strength" part="density-strength" type="range" min="0.1" max="1" step="0.05" .value=${String(s.densityStrength)}
              @input=${(e: Event) => change({densityStrength: number(e)})} /><span class="readout">${Math.round(s.densityStrength * 100)}%</span></div>
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
        <label for="point-opacity">Opacity</label>
        <div class="with-readout"><input id="point-opacity" part="point-opacity" type="range" min="0.1" max="1" step="0.05" .value=${String(opacity)} ?disabled=${!s.points}
          @input=${(e: Event) => change({pointOpacity: number(e)})} /><span class="readout">${Math.round(opacity * 100)}%</span></div>
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

  /**
   * The Resolution slider, coarse to fine: one stop per depth the cell sizes ask for at the map's
   * zoom, so sizes that would draw the same cells are one stop. The stops past the finest the map
   * can ask for at its camera are struck through on the track. Moving the slider onto one keeps the
   * size asked for, which the map draws once the camera lets it, and shows the slider on the finest
   * it can draw now. The readout gives the size of the cells drawn at the stop shown.
   */
  private resolutionControl(change: (patch: Partial<DisplaySettings>) => void): TemplateResult {
    const zoom = this.map?.zoom ?? 0;
    // Before the map has a camera and `meta`, every size is a stop of its own.
    const stops = this.map?.densityStops() ?? [];
    const levels: {px: number; depth: number; enabled: boolean}[] = [];
    for (const stop of stops.length > 0 ? stops : DENSITY_CELL_SIZES.map((px) => ({px, depth: cellDepth(zoom, px), enabled: true}))) {
      const prev = levels[levels.length - 1];
      if (prev && prev.depth === stop.depth) prev.enabled ||= stop.enabled;
      else levels.push({...stop});
    }
    const last = levels.length - 1;
    const finest = Math.max(0, levels.map((l) => l.enabled).lastIndexOf(true));
    const asked = cellDepth(zoom, nearestStop(this.densityResolution));
    const index = levels.findIndex((l) => l.depth === asked);
    const at = Math.min(index >= 0 ? index : asked < levels[0]!.depth ? 0 : last, finest);
    const drawn = Math.round((WORLD_SIZE * 2 ** zoom) / 2 ** levels[at]!.depth);
    // The track runs between the thumb's centres at either end; the struck part starts half a stop past the finest.
    const past = finest < last ? html`<span class="past" style=${`left:calc(8px + (100% - 16px) * ${(finest + 0.5) / last})`}></span>` : nothing;
    const onInput = (e: Event) => {
      const input = e.target as HTMLInputElement;
      const i = Number(input.value);
      input.value = String(Math.min(i, finest));
      change({densityResolution: levels[i]!.px});
    };
    return html`<label for="density-resolution">Resolution</label>
      <div class="resolution">
        <input id="density-resolution" part="density-resolution" type="range" min="0" max=${last} step="1" .value=${String(at)}
          aria-valuetext=${`Cells about ${drawn} px across`} @input=${onInput} />${past}
      </div>
      <span></span>
      <div class="ends"><span>Coarse</span><span class="readout">cells ≈ ${drawn} px</span><span>Fine</span></div>`;
  }

  /** The Scale choice under Resolution: Linear or Log, the scale density's colours follow. */
  private densityScaleControl(set: string, change: (patch: Partial<DisplaySettings>) => void): TemplateResult {
    const scale = drawnDensityScale(set);
    const at = DENSITY_SCALES.findIndex((x) => x.scale === scale);
    return html`<span id="density-scale-label">Scale</span>
      <div part="density-scale" class="seg" role="radiogroup" aria-labelledby="density-scale-label">
        ${DENSITY_SCALES.map(
          (x, i) => html`<button type="button" role="radio" data-scale=${x.scale} aria-checked=${x.scale === scale ? 'true' : 'false'} tabindex=${i === at ? '0' : '-1'}
            @click=${() => change({densityScale: x.scale})}
            @keydown=${(e: KeyboardEvent) => radioKeys(e, DENSITY_SCALES.length, i, (j) => change({densityScale: DENSITY_SCALES[j]!.scale}))}>${x.label}</button>`
        )}
      </div>`;
  }

  /**
   * The number column the points are sized by, or null for one size, as the store draws them. The
   * explorer's own properties only send changes on, through its map.
   */
  private get sizedBy(): string | null {
    return this.resolvedStore?.get('legend').sizeBy ?? null;
  }

  /** The size range and scale the map draws with: the choices every element over the store shares. */
  private get sizing(): Sizing {
    return sizingOf(this.resolvedStore);
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
      <button part="size-by" class="ramp-choice" type="button" aria-haspopup="menu" aria-expanded=${this.sizeMenuOpen ? 'true' : 'false'} aria-label=${`Size by: ${sizeBy === null ? 'None' : columnCaption(sizeBy)}`}
        ?disabled=${disabled || (sizeBy === null && this.sizeColumns.length === 0)} title=${this.sizeColumns.length === 0 ? 'No number column to size by' : nothing}
        @click=${() => (this.sizeMenuOpen = !this.sizeMenuOpen)} @keydown=${this.onSizeByKey}><span class="t">${sizeBy === null ? 'None' : columnCaption(sizeBy)}</span>${icon('chev', 12, 1.4)}</button>`;
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
    // One entry is in the tab order, the one focused last, as in a radio group.
    const keys = (e: KeyboardEvent, i: number) => {
      const items = Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="size-option"]'));
      const next = {ArrowDown: (i + 1) % items.length, ArrowUp: (i - 1 + items.length) % items.length, Home: 0, End: items.length - 1}[e.key];
      if (e.key === 'Escape') {
        e.stopPropagation();
        this.closeSizeMenu(true);
        return;
      }
      if (next === undefined) return;
      e.preventDefault();
      items.forEach((item, j) => (item.tabIndex = j === next ? 0 : -1));
      items[next]?.focus();
    };
    const checked = Math.max(0, options.findIndex((o) => (o.value || null) === sizeBy));
    return html`<div part="size-menu" class="size-menu" popover="manual" role="menu" aria-labelledby="size-menu-label" @focusout=${this.onSizeMenuFocusOut}>
      <div class="hd" id="size-menu-label">Size by</div>
      ${options.map(
        (o, i) => html`<button part="size-option" type="button" role="menuitemradio" data-value=${o.value} aria-checked=${(o.value || null) === sizeBy ? 'true' : 'false'}
          tabindex=${i === checked ? '0' : '-1'} @click=${() => choose(o.value)} @keydown=${(e: KeyboardEvent) => keys(e, i)}><span>${o.title}</span><span class="kind">${o.kind}</span></button>`
      )}
    </div>`;
  }

  /** Close the Size by menu, putting focus back on its button with `refocus`. */
  private closeSizeMenu(refocus: boolean): void {
    this.sizeMenuOpen = false;
    if (refocus) void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="size-by"]')?.focus());
  }

  /** The arrow keys on the Size by button open its menu, as a menu button's do. */
  private onSizeByKey = (e: KeyboardEvent): void => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    e.preventDefault();
    this.sizeMenuOpen = true;
  };

  /** Focus leaving the Size by menu, by Tab or otherwise, closes it; Tab goes on from where focus went. */
  private onSizeMenuFocusOut = (e: FocusEvent): void => {
    const to = e.relatedTarget as Node | null;
    if (to && (e.currentTarget as HTMLElement).contains(to)) return;
    this.closeSizeMenu(false);
  };

  /** A press in the Layers popover outside the Size by button closes the Size by menu. */
  private onPopoverPress = (e: PointerEvent): void => {
    if (!this.sizeMenuOpen) return;
    const button = this.renderRoot.querySelector('[part="size-by"]');
    if (button && !e.composedPath().includes(button)) this.sizeMenuOpen = false;
  };

  /**
   * Send a change to Size by, the size range or the scale through the explorer's properties to its
   * map, and report all four as they will stand.
   */
  private changeSize(patch: {sizeBy?: string | null; min?: number; max?: number; scale?: SizeScale}): void {
    const now = this.sizing;
    if (patch.sizeBy !== undefined) this.sizeBy = patch.sizeBy ?? 'none';
    if (patch.min !== undefined) this.sizeMin = patch.min;
    if (patch.max !== undefined) this.sizeMax = patch.max;
    if (patch.scale !== undefined) this.sizeScale = patch.scale;
    emit(this, 'tessera-sizechange', {
      sizeBy: patch.sizeBy !== undefined ? patch.sizeBy : this.sizedBy,
      min: patch.min ?? now.min,
      max: patch.max ?? now.max,
      scale: patch.scale ?? now.scale
    });
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
    if (patch.densityResolution !== undefined) this.densityResolution = patch.densityResolution;
    if (patch.densityScale !== undefined) this.densityScale = patch.densityScale;
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
    this.toggleAttribute('data-compact', this.compact);
    this.placeSizeMenu(changed.has('sizeMenuOpen'));
    this.followLayout();
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

  /** A click on empty map closes the card that is not pinned and drops the selection; pinned cards stay. */
  private onMiss(): void {
    const s = this.resolvedStore;
    const sel = s?.get('selection');
    if (s && sel && (sel.item || sel.artifact || sel.itemRefusal || sel.artifactRefusal)) s.clearSelection();
    const m = this.map;
    if (m) m.lastPick = null;
    this.requestUpdate();
  }

  /**
   * A card's close button: close the card, and drop the selection where the card shows it. A pinned
   * card closes alone unless it shows what is selected.
   */
  private closeDetail(e: Event): void {
    const refocus = () => void this.updateComplete.then(() => this.map?.focus());
    const s = this.resolvedStore;
    if (!s) return;
    const from = e.composedPath().find((n): n is HTMLElement => n instanceof HTMLElement && n.dataset.callout !== undefined);
    const key = from?.dataset.callout;
    // A card beside a point that closes gives focus back to the map rather than to the page.
    if (from) refocus();
    const live = this.liveKey();
    if (key && key !== 'live') {
      this.pinned = this.pinned.filter((p) => p.key !== key);
      if (key !== live) return;
    } else if (live) this.pinned = this.pinned.filter((p) => p.key !== live);
    this.map?.clearPick();
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
