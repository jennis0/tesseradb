import {ContextProvider} from '@lit/context';
import {css, html, nothing, svg, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import type {ArtifactDetail, ItemDetail, PaletteName, Store} from '@mosaicajs/client';
import type {CategoryPaletteName, Colouring, DensityColours, DensityMode, DensityScale, RampName, RampScale, SizeScale, Sizing} from '@mosaicajs/deck';
import {CATEGORY_PALETTES, DEFAULT_DENSITY_CELL_PX, DEFAULT_DENSITY_SCALE, DENSITY_CELL_SIZES, RAMPS, cellDepth, nearestStop} from '@mosaicajs/deck';
import {CLUSTER_PREFIX, PALETTES, WORLD_SIZE, activeCount, artifactName, colourLayers, emptyDraft} from '@mosaicajs/client';
import {hasOneLayout, sizesPoints} from '@mosaicajs/client/internal';
import {DENSITY_COLOUR_TITLES, colourOfFraction, css as rgb, hexOf} from '@mosaicajs/deck/internal';
import {MosaicaElement, columnCaption, emit, shortCount} from './base.js';
import {placeCallout, type Rect, type Side} from './callout.js';
import {colouringOf, setColouring, sizingOf, watchChoices} from './colouring.js';
import {densityGradient, displayStyles, radioKeys, type DisplaySettings} from './display.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon, type IconName} from './icons.js';
import {exportparts, forwarded} from './parts.js';
import {sameFrame} from './view-switch.js';
import {drawnDensityColours, drawnDensityScale, type MosaicaMap} from './map.js';
import {chrome, tokens} from './tokens.js';
import './map.js';
import './status.js';
import './filter-panel.js';
import './item-card.js';
import './selection.js';
import './layer-picker.js';
import './view-picker.js';
import './key-picker.js';
import './artifact-card.js';
import './colour-editor.js';
import type {MosaicaColourEditor} from './colour-editor.js';

/** Every part of every element the explorer renders, forwarded. */
const FORWARD = {
  map: exportparts('map'),
  status: exportparts('status'),
  'view-picker': exportparts('view-picker'),
  'key-picker': exportparts('key-picker'),
  'layer-picker': exportparts('layer-picker'),
  'filter-panel': exportparts('filter-panel', [...forwarded('field-card'), ...forwarded('filter'), ...forwarded('cluster-filter')]),
  selection: exportparts('selection'),
  'item-card': exportparts('item-card'),
  'artifact-card': exportparts('artifact-card'),
  'colour-editor': exportparts('colour-editor', [...forwarded('filter'), ...forwarded('cluster-filter')])
};

const ALL_PANELS = ['toolbar', 'legend', 'filters', 'selection', 'detail'] as const;
/** The container widths, in pixels, between which the explorer takes its compact form. */
const COMPACT_BETWEEN = [720, 1000] as const;
type Panel = (typeof ALL_PANELS)[number];
type Sheet = 'filters' | 'layers' | 'detail';
/** The menus the Layers popover opens beside itself, each from the button of the same name. */
type Menu = 'size' | 'colour' | 'palette' | 'ramp';
/** The button each menu opens from. */
const MENU_BUTTON: Record<Menu, string> = {size: 'size-by', colour: 'colour-by', palette: 'palette', ramp: 'ramp'};
/** A number of points as Most points says it: `250K`, `2M`. */
const pointsText = shortCount;
/** `n` to two significant figures, so the slider stops on round numbers. */
const roundPoints = (n: number) => {
  const unit = 10 ** Math.max(0, Math.floor(Math.log10(n)) - 1);
  return Math.round(n / unit) * unit;
};
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
/** How far along its track a slider's value is, for its filled part. */
const fill = (v: number, min: number, max: number) => `--fill:${(Math.min(1, Math.max(0, (v - min) / (max - min))) * 100).toFixed(1)}%`;
/** A ramp as a CSS gradient, low to high. */
const rampGradient = (ramp: RampName, reverse: boolean) => `linear-gradient(to right, ${Array.from({length: 8}, (_, k) => rgb(colourOfFraction(k / 7, ramp, reverse))).join(', ')})`;

/** The Density choices in the Layers popover, in order. */
const DENSITY_MODES: readonly {mode: DensityMode; label: string}[] = [
  {mode: 'none', label: 'None'},
  {mode: 'smooth', label: 'Smooth'},
  {mode: 'hex', label: 'Hex'},
  {mode: 'grid', label: 'Grid'},
  {mode: 'contours', label: 'Lines'}
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
  {sheet: 'filters', icon: 'filter', label: 'Fields', panel: 'filters'},
  {sheet: 'layers', icon: 'layers', label: 'Layers', panel: 'legend'},
  {sheet: 'detail', icon: 'info', label: 'Item', panel: 'detail'}
];

/**
 * The map with its status strip, toolbar, field cards, display settings, layers, selection and item
 * card, laid out together. It builds its own store from `viewer-url` and `token` or an `authorise`
 * property, or takes a `store` property, and provides it by context to everything inside it,
 * including elements a host puts in its slots.
 *
 * The heading names what is shown: `dataset-title` where the host sets one, with the view's name
 * under it, else the view's name alone. A view's name that is only its id is left out. With
 * several views the view's name is the view choice.
 *
 * The left holds the heading and the field column (`<mosaica-filter-panel>`): what the cards count,
 * In view or Highlighted beside All matching, the clauses applied as chips with Clear all, one
 * `<mosaica-field-card>` per field, and Add field. `layout="docked"` puts them in a sidebar left of
 * the map; `layout="overlay"` puts them in a card over the map's top-left, which scrolls inside
 * itself when tall. The map's tools sit at the top-left of the map, right of the card in the
 * overlay layout, with a Layers button beneath them. The status strip sits in the map's
 * bottom-right.
 *
 * Picking a point opens its item card beside the point, joined to it by a short leader: on the side
 * with most room, clear of the cards over the map, following the point as the map pans and zooms,
 * hidden while the point is off the map, and clear of the other cards beside points. The card shows
 * the headline, the `subtitle-field` field, three fields and "Show all N fields". Picking another
 * point replaces it unless Pin was pressed, which keeps it open; Close or Escape closes it and drops
 * the selection, and a click on the map that finds nothing does the same for the card that is not
 * pinned. Tab from the map goes to the card, which is named by its item's title and gives focus back
 * to the map as it closes. A picked cluster's card behaves the same way. While a region is selected
 * its card stands in the map's top-right corner, and where the map holds no position for the
 * selection, as after following an item into another view, the item's card goes there too.
 *
 * The Layers button opens a popover in four sections. Points: whether the points are drawn; Most
 * points, a slider from `budget-min` to `budget-max` on a log scale that sets the store's budget
 * when it is let go (`Store.setBudget`); while a `nested` or `dag` layer is drawn, Most clusters,
 * a slider from `cluster-budget-min` to `cluster-budget-max` on a log scale that sets the most
 * clusters such a layer is cut to when it is let go (`Store.setClusterBudget`); what sizes the
 * points; and their opacity. Size by lists
 * None and the number columns the points arrive with. Under None one Size slider sets every
 * point's radius, which `hide-size` leaves out; under a column two sliders set the radii of its
 * smallest and largest value, and a Linear, Log or Rank choice places values between them. Colour:
 * Colour by, which lists None, every layer that can colour (a levelled layer once per level) and
 * the rendered category and number columns; then for a layer or a category the palette, and for a
 * number the ramp, Linear or Log and Reverse, unless `hide-palettes` is set. A layer's palette is
 * the store's (`Store.setPalette`), whose size the clusters' slots are served for. While the
 * points are coloured by a category or a layer, Edit colours opens `<mosaica-colour-editor>` over
 * the page, listing the values or the clusters drawn (at the level coloured, on a levelled layer)
 * with their colours to choose and reset, in an order that holds through a pan or a filter. A field card's paint button chooses Colour by too. Density: how density is drawn, at what resolution, in which
 * colours and how strongly. Layers: the layer picker. The display settings are the explorer's properties of
 * the same names, passed to its map. `pinned-filters` names the columns whose cards are listed
 * before they hold a clause.
 *
 * In a container narrower than 1000 px, either layout takes the overlay's form with a narrower card
 * of 264 px, every field card folded to one line until one is opened, and opening one folds the
 * one open before; the tools move to the bottom-left and the strip shortens its figures. In a
 * container 720 px wide or narrower, the status strip runs full width above a tab bar (Fields,
 * Layers, Item), and each tab opens its panel as a sheet: the field column, folded as in the
 * compact form; the Layers popover's sections; and the item card. The cards over the map are not
 * drawn there.
 *
 * Every event its elements fire bubbles out of it, since each is composed.
 *
 * @summary The map and every panel, in a default layout.
 * @tagname mosaica-explorer
 * @category Elements
 * @slot toolbar - Replaces the view picker and the key picker.
 * @slot layers - Replaces the layer picker, in the Layers popover.
 * @slot filters - Replaces the field column.
 * @slot selection - Replaces the selection panel, shown while a region is selected.
 * @slot detail - Replaces the item and artifact cards, in the card beside the point.
 * @slot status - Replaces the status strip drawn on the map.
 * @slot tooltip - Replaces the map's hover tooltip.
 * @slot top-right - Content in the map's top-right corner, above the selection's card.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-viewchange']>} mosaica-viewchange - The map's camera moved.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-pick']>} mosaica-pick - A point was clicked, and again with its record.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-miss']>} mosaica-miss - A click on the map found nothing; the explorer closes the card that is not pinned and drops the selection.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-hover']>} mosaica-hover - The pointer moved over a point.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-artifactopen']>} mosaica-artifactopen - An artifact was opened and its drill-down arrived.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-selectchange']>} mosaica-selectchange - A selection was drawn, changed or cleared, or its counts arrived.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-layerchange']>} mosaica-layerchange - The layers chosen changed.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-colourchange']>} mosaica-colourchange - Colour by changed, in the Layers popover or by a card's paint button.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-levelchange']>} mosaica-levelchange - The level a layer is coloured and labelled at changed.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-displaychange']>} mosaica-displaychange - A setting in the Points or Density section changed.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-sizechange']>} mosaica-sizechange - Size by, the size range or the scale changed in the Points section.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-budgetchange']>} mosaica-budgetchange - Most points was let go at a new number.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-clusterbudgetchange']>} mosaica-clusterbudgetchange - Most clusters was let go at a new number.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-fold']>} mosaica-fold - A field card was folded to one line or opened.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-valuecolour']>} mosaica-valuecolour - Colours were chosen or reset for values of a category, on a field card or in Edit colours.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-clustercolour']>} mosaica-clustercolour - Colours were chosen or reset for clusters of a layer, on a field card or in Edit colours.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-palettechange']>} mosaica-palettechange - The palette, the ramp, its scale or its direction was chosen in the Colour section.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-clusterpalettechange']>} mosaica-clusterpalettechange - A palette was chosen for a layer's clusters in the Colour section.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-statechange']>} mosaica-statechange - The status strip's panel state changed.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-expired']>} mosaica-expired - The session expired.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-filterchange']>} mosaica-filterchange - A field card or chip changed a clause.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-clausechange']>} mosaica-clausechange - A `member_of` clause was put on or taken off.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-artifactfit']>} mosaica-artifactfit - Fit was pressed on the artifact card; the explorer fits its map to the artifact.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-open']>} mosaica-open - Open was pressed on the item card.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-close']>} mosaica-close - A card's close button was pressed; the explorer closes the card and drops the selection it shows.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-viewswitch']>} mosaica-viewswitch - The view changed through the view or key picker.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-viewfollow']>} mosaica-viewfollow - A view chip on the item card was pressed; the explorer switches to that view and centres on the item.
 * @csspart frame - The explorer's grid.
 * @csspart sidebar - The sidebar, in the docked layout.
 * @csspart panel - The card over the map's top-left, in the overlay layout and in a container
 *   narrower than 1000 px.
 * @csspart dataset-title - The heading's dataset title, where `dataset-title` is set.
 * @csspart view-name - The view's name in the heading, where there is one view to show and its name
 *   is not its id.
 * @csspart selection-card - The card in the map's top-right corner holding the selection, while a
 *   region is selected.
 * @csspart callout - A card beside a point on the map, with `data-side` (`right`, `left`, `below`
 *   or `above`) and `data-pinned` while pinned.
 * @csspart pin - A callout's Pin button, with `aria-pressed`.
 * @csspart leaders - The lines joining each callout to its point.
 * @csspart layers-toggle - The Layers button under the map's tools, with `data-on` while any layer
 *   is drawn.
 * @csspart layers-popover - The popover holding the display sections and the layer picker, while it
 *   is open.
 * @csspart display - The Points, Colour and Density sections, at the top of the Layers popover.
 * @csspart points-toggle - The Points switch, with `aria-checked`.
 * @csspart most-points - The Most points slider.
 * @csspart most-points-value - The number of points it stands at, such as "250K".
 * @csspart most-clusters - The Most clusters slider, while a `nested` or `dag` layer is drawn.
 * @csspart most-clusters-value - The number of clusters it stands at, such as "1K".
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
 * @csspart colour-by - The Colour by button, naming what the points are coloured by, with
 *   `aria-expanded` while its menu is open.
 * @csspart colour-menu - The Colour by menu, while it is open.
 * @csspart colour-option - An entry in the Colour by menu, with `data-value` (empty for None,
 *   `cluster:<layer>@<level>` for one level of a levelled layer) and `aria-checked`.
 * @csspart palette - The Palette button, while the points are coloured by a layer or a category.
 * @csspart palette-menu - The Palette menu, while it is open: a row per palette, each a strip of its
 *   colours, its name and a line saying how many it has.
 * @csspart palette-option - A palette in that menu, with `data-value` and `aria-checked`.
 * @csspart edit-colours - The Edit colours button, enabled while the points are coloured by a
 *   category or a layer.
 * @csspart ramp - The Ramp button, while the points are coloured by a number or a date.
 * @csspart ramp-menu - The Ramp menu, while it is open.
 * @csspart ramp-option - A ramp in that menu, with `data-value` and `aria-checked`.
 * @csspart ramp-scale - The Linear and Log choice of the ramp's scale, each with `data-scale` and
 *   `aria-checked`.
 * @csspart ramp-reverse - The Reverse button, with `aria-pressed`.
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
 * @csspart map-<part> - A part of the inner `<mosaica-map>`, such as `map-controls`.
 * @csspart status-<part> - A part of an inner `<mosaica-status>`.
 * @csspart view-picker-<part> - A part of the inner `<mosaica-view-picker>`.
 * @csspart key-picker-<part> - A part of the inner `<mosaica-key-picker>`.
 * @csspart layer-picker-<part> - A part of the inner `<mosaica-layer-picker>`.
 * @csspart filter-panel-<part> - A part of the inner `<mosaica-filter-panel>`.
 * @csspart field-card-<part> - A part of a `<mosaica-field-card>` in the field column.
 * @csspart filter-<part> - A part of a `<mosaica-filter>` search box on a card.
 * @csspart cluster-filter-<part> - A part of a `<mosaica-cluster-filter>` search box on a card.
 * @csspart selection-<part> - A part of the inner `<mosaica-selection>`.
 * @csspart item-card-<part> - A part of the inner `<mosaica-item-card>`, such as `item-card-title`.
 * @csspart artifact-card-<part> - A part of the inner `<mosaica-artifact-card>`.
 * @csspart colour-editor-<part> - A part of the inner `<mosaica-colour-editor>`, such as
 *   `colour-editor-dialog`.
 * @cssprop --mosaica-explorer-height - The explorer's height.
 * @cssprop --mosaica-sidebar-width - The width of the docked sidebar and of the card over the map.
 */
export class MosaicaExplorer extends MosaicaElement {
  static override styles = [
    tokens,
    chrome,
    displayStyles,
    css`
      :host {
        display: block;
        container-type: inline-size;
        container-name: explorer;
        background: var(--_mosaica-map-bg);
        height: var(--mosaica-explorer-height, 100%);
        min-height: 320px;
        --_panel-width: var(--mosaica-sidebar-width, 340px);
        --_right-width: 320px;
      }
      /* The compact form's corners are nearer the edges. */
      :host([data-compact]) {
        --_mosaica-space-base: 12px;
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
        --_panel-width: min(var(--mosaica-sidebar-width, 340px), 264px);
        --_right-width: 272px;
        --_mosaica-tool-size: 30px;
      }
      .stage {
        position: relative;
        min-width: 0;
        min-height: 0;
        container-type: size;
        container-name: stage;
      }
      mosaica-map,
      ::slotted(mosaica-map) {
        height: 100%;
        min-height: 320px;
      }
      /* What the map draws at its top-left, its tools or a region's tag, sits right of the card; in
         the narrow layout there is no card. */
      @container explorer (width > 720px) {
        .floating mosaica-map {
          --mosaica-map-inset-left: calc(var(--_panel-width) + var(--_mosaica-space));
        }
      }
      [part='sidebar'] {
        display: flex;
        flex-direction: column;
        overflow-y: auto;
        border-right: 1px solid var(--_mosaica-line);
        background: var(--_mosaica-surface);
        --_mosaica-panel-rule: var(--_mosaica-line);
      }
      [part='sidebar'] .head {
        border-bottom-color: var(--_mosaica-line);
      }
      /* The card over the map: the sidebar folded up. */
      [part='panel'] {
        position: absolute;
        z-index: 1;
        top: var(--_mosaica-space);
        left: var(--_mosaica-space);
        width: var(--_panel-width);
        max-height: calc(100% - 2 * var(--_mosaica-space));
        display: flex;
        flex-direction: column;
        overflow-y: auto;
        --_mosaica-panel-padding: 12px 14px 14px;
      }
      [part='panel'] mosaica-filter-panel,
      [part='sidebar'] mosaica-filter-panel {
        --_mosaica-panel-inline: 14px;
      }
      .compact [part='panel'] {
        --_mosaica-panel-padding: 10px 12px;
      }
      .compact [part='panel'] mosaica-filter-panel {
        --_mosaica-panel-inline: 12px;
      }
      /* Room beneath for the tools and the Layers button in the bottom-left corner. */
      .compact [part='panel'] {
        max-height: calc(100% - 2 * var(--_mosaica-space) - 200px);
      }
      /* A typeahead in the card opens over what sits below it, and the card scrolls to show it. */
      [part='panel'] mosaica-filter-panel,
      [part='sidebar'] mosaica-filter-panel {
        position: relative;
      }
      .card {
        background: var(--_mosaica-surface);
        border: 1px solid var(--_mosaica-line);
        border-radius: var(--_mosaica-radius);
        box-shadow: var(--_mosaica-shadow);
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
      /* The field column's subject row draws the rule under the heading. */
      .head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 10px;
        padding: 14px 16px 10px;
      }
      [part='panel'] .head {
        padding: 12px 14px 10px;
      }
      .compact [part='panel'] .head {
        padding: 10px 12px 8px;
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
        font-size: 14px;
      }
      .sub {
        font-size: 12px;
        color: var(--_mosaica-ink-2);
      }
      .sub .pickers {
        gap: 2px;
      }
      .sub mosaica-view-picker::part(field) {
        font-size: 12px;
        font-weight: 400;
        line-height: 1.45;
      }
      .compact mosaica-view-picker::part(field) {
        font-size: 13px;
      }
      .compact .sub mosaica-view-picker::part(field) {
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
        background: var(--_mosaica-surface);
        border: 1px solid var(--_mosaica-line);
        border-radius: var(--_mosaica-radius);
        box-shadow: var(--_mosaica-shadow);
      }
      [part='layers-toggle'] {
        position: relative;
        width: var(--_mosaica-tool-size, 32px);
        height: var(--_mosaica-tool-size, 32px);
        display: grid;
        place-items: center;
        border-radius: var(--_mosaica-radius-control);
        color: color-mix(in srgb, var(--_mosaica-ink) 82%, var(--_mosaica-surface));
      }
      [part='layers-toggle']:hover,
      [part='layers-toggle'][aria-expanded='true'] {
        background: var(--_mosaica-surface-2);
      }
      [part='layers-toggle'] .on {
        position: absolute;
        top: 3px;
        right: 3px;
        width: 6px;
        height: 6px;
        border-radius: 50%;
        background: var(--_mosaica-accent);
      }
      [part='layers-popover'] {
        position: absolute;
        top: 0;
        left: calc(100% + 8px);
        width: 320px;
        max-height: min(560px, calc(100cqh - 2 * var(--_mosaica-space)));
        overflow-y: auto;
        box-shadow: 0 6px 24px rgba(0, 0, 0, 0.1);
      }
      [part='layers-popover'] mosaica-layer-picker,
      [part='sheet'] mosaica-layer-picker {
        display: block;
        border-top: 1px solid var(--_mosaica-line-2);
        --_mosaica-panel-padding: 12px 14px 14px;
      }
      .compact [part='layers-popover'] {
        top: auto;
        bottom: 0;
      }
      [part='detail'],
      [part='selection-card'] {
        --_mosaica-panel-padding: 12px 14px;
        width: var(--_right-width);
        max-width: 100%;
        min-height: 0;
        overflow-y: auto;
      }
      [part='detail'] {
        flex: 0 1 auto;
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
        stroke: var(--_mosaica-ink);
        stroke-width: 1.5;
      }
      [part='leaders'] circle {
        fill: var(--_mosaica-surface);
        stroke: var(--_mosaica-ink);
        stroke-width: 1.5;
      }
      [part~='callout'] {
        position: absolute;
        left: 0;
        top: 0;
        width: 300px;
        pointer-events: auto;
        box-shadow: 0 8px 28px rgba(0, 0, 0, 0.14);
        --_mosaica-panel-padding: 12px 14px 10px;
        --_mosaica-panel-inline: 14px;
        --_mosaica-panel-rule: transparent;
        --_mosaica-key-width: 104px;
        --_mosaica-field-gap: 4px 10px;
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
        color: var(--_mosaica-ink-2);
      }
      [part='pin']:hover {
        background: var(--_mosaica-surface-2);
      }
      [part='pin'][aria-pressed='true'] {
        background: var(--_mosaica-surface-3);
        color: var(--_mosaica-ink);
      }
      /* The selection keeps its heading, counts and actions; only its list of marks scrolls, so the
         detail card under it stays on screen. */
      [part='selection-card'] {
        flex: none;
        overflow: visible;
      }
      [part='selection-card'] mosaica-selection::part(items) {
        max-height: min(144px, 18cqh);
        overflow-y: auto;
      }
      .compact [part='detail'] mosaica-item-card,
      .compact [part='detail'] mosaica-artifact-card {
        font-size: 12px;
      }
      .compact [part='detail'] {
        --_mosaica-panel-padding: 12px 12px 10px;
        --_mosaica-title-size: 14px;
        --_mosaica-key-width: 104px;
        --_mosaica-field-gap: 4px 10px;
      }
      .right {
        display: flex;
        flex-direction: column;
        align-items: flex-end;
        gap: calc(var(--_mosaica-space) / 2);
        max-height: calc(100cqh - 2 * var(--_mosaica-space) - 48px);
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
          border-top: 1px solid var(--_mosaica-line);
          background: var(--_mosaica-surface);
        }
        [part='tabs'] button {
          flex: 1 1 0;
          min-width: 0;
          display: flex;
          flex-direction: column;
          align-items: center;
          justify-content: center;
          gap: 4px;
          color: var(--_mosaica-ink-2);
          font-size: 11px;
          font-weight: 500;
        }
        [part='tabs'] button[aria-selected='true'] {
          color: var(--_mosaica-ink);
          font-weight: 600;
        }
        [part='sheet'] {
          position: absolute;
          left: 0;
          right: 0;
          bottom: 56px;
          max-height: 70%;
          overflow-y: auto;
          background: var(--_mosaica-surface);
          border-top: 1px solid var(--_mosaica-line);
          border-radius: 12px 12px 0 0;
          box-shadow: var(--_mosaica-shadow);
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
          border-top: 1px solid var(--_mosaica-line-2);
          background: var(--_mosaica-surface);
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
          background: var(--_mosaica-line);
          margin: 8px auto 0;
        }
        [part='strip-row'] {
          display: block;
        }
        [part='strip-row'] mosaica-status {
          display: block;
        }
        [part='strip-row'] mosaica-status::part(strip) {
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
   * Which regions appear, space- or comma-separated, from `toolbar` (the heading), `legend` (the
   * Layers button and its popover), `filters` (the field column), `selection` and `detail` (the
   * item and artifact cards). Defaults to all five. Any other name, such as `hierarchy`, shows
   * nothing.
   */
  @property() accessor panels: string = ALL_PANELS.join(' ');
  /** Passed to the map's `colour-by`. */
  @property({attribute: 'colour-by'}) accessor colourBy = '';
  /** Passed to the map's `layers`. */
  @property() accessor layers = '';
  /** Passed to the map's `tooltip-fields`. */
  @property({attribute: 'tooltip-fields'}) accessor tooltipFields = '';
  /** The field that titles a point, in the map's tooltip and the item card's headline. Unset, the title is the `mosaica_id`. */
  @property({attribute: 'title-field'}) accessor titleField = '';
  /** The field the item card shows under its headline. Unset, it shows none. */
  @property({attribute: 'subtitle-field'}) accessor subtitleField = '';
  /**
   * How many marks to aim for on screen, passed to the map's `budget`. `0`, the default, leaves the
   * store's own, which starts at 250000. Most points in the Layers popover sets the store's budget
   * itself.
   */
  @property({type: Number}) accessor budget = 0;
  /** The fewest marks Most points offers, passed to the map's `budget-min`. */
  @property({type: Number, attribute: 'budget-min'}) accessor budgetMin = 1_000;
  /** The most marks Most points offers, passed to the map's `budget-max`. */
  @property({type: Number, attribute: 'budget-max'}) accessor budgetMax = 2_000_000;
  /**
   * The most clusters a `nested` or `dag` layer is cut to, passed to the map's `cluster-budget`;
   * `0` is the finest cut. Most clusters in the Layers popover sets the store's itself.
   */
  @property({type: Number, attribute: 'cluster-budget'}) accessor clusterBudget = 1_000;
  /** The fewest clusters Most clusters offers. */
  @property({type: Number, attribute: 'cluster-budget-min'}) accessor clusterBudgetMin = 10;
  /** The most clusters Most clusters offers. */
  @property({type: Number, attribute: 'cluster-budget-max'}) accessor clusterBudgetMax = 10_000;
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
  /**
   * Passed to the map's `palette`, the palette a layer's clusters are coloured from; the Palette
   * choice in the Colour section changes it while the points are coloured by a layer.
   */
  @property() accessor palette: PaletteName | '' = '';
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
  /** Passed to the map's `clusterColours`. */
  @property({attribute: false}) accessor clusterColours: Record<string, Record<string, string>> | null = null;
  /** The dataset's title, which heads the explorer above the view's name. Unset, the view's name is the heading. */
  @property({attribute: 'dataset-title'}) accessor datasetTitle = '';
  /** Passed to the field column's `pinned`: the columns whose cards are listed before they hold a clause. */
  @property({attribute: 'pinned-filters'}) accessor pinnedFilters = '';
  /** Leaves the palette and ramp choices out of the Colour section, for a host that sets them. */
  @property({type: Boolean, attribute: 'hide-palettes'}) accessor hidePalettes = false;
  /** The menu open beside the Layers popover. @internal */
  @state() accessor menu: Menu | null = null;
  /** Where Most points is being dragged, before it is let go. @internal */
  @state() accessor pointsDragged: number | null = null;
  /** Where Most clusters is being dragged, before it is let go. @internal */
  @state() accessor clustersDragged: number | null = null;
  /** Whether the list of density colours in the Layers popover is open. @internal */
  @state() accessor densityColoursOpen = false;
  /** @internal */
  @state() accessor sheet: Sheet | null = null;
  /** The narrow layout's tab focused last, which keeps the tab list's one place in the tab order. */
  @state() private accessor tabFocus: Sheet | null = null;
  /** The level a layer is coloured and labelled at, chosen on its card or in Colour by. @internal */
  @state() accessor level: number | null = null;
  /** The cards kept open by Pin, oldest first. @internal */
  @state() accessor pinned: Pinned[] = [];
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
    this.closeLayers();
    super.disconnectedCallback();
  }

  /** Dispose of the store the explorer built and of its map's GPU resources. */
  override dispose(): void {
    this.following?.();
    this.map?.dispose();
    super.dispose();
  }

  /** The `<mosaica-map>` the explorer renders, for a host that calls `fit`, `fitTo` or `select`. */
  get map(): MosaicaMap | null {
    return this.renderRoot?.querySelector<MosaicaMap>('mosaica-map') ?? null;
  }

  private has(panel: Panel): boolean {
    return this.panels.split(/[\s,]+/).includes(panel);
  }

  /** A press anywhere outside the Layers button, its popover and the Edit colours dialog it opens closes the popover. */
  private onOutside = (e: PointerEvent): void => {
    const path = e.composedPath();
    const group = this.renderRoot.querySelector('.layers');
    const editor = this.renderRoot.querySelector('mosaica-colour-editor');
    if (group && !path.includes(group) && !(editor && path.includes(editor))) this.closeLayers();
  };

  private openLayers(): void {
    this.layersOpen = true;
    document.addEventListener('pointerdown', this.onOutside, true);
  }

  private closeLayers(): void {
    this.layersOpen = false;
    this.menu = null;
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
    const level = this.level;
    // A click that found nothing shows no card; a broken pick shows its fault.
    const hasDetail = Boolean(selection?.item || selection?.artifact || selection?.artifactRefusal || selection?.itemRefusal || this.map?.lastPick?.kind === 'broken');
    // The detail shows whichever changed last.
    const showArtifact = this.showsArtifact();
    const detail = this.liveCard(showArtifact);
    // The pickers draw nothing for a one-view bundle, so their row is hidden there unless the host
    // filled the slot. The slot is always rendered, so its slotchange keeps `toolbarFilled` current.
    const pickersShown = this.toolbarFilled || (meta !== null && !hasOneLayout(meta));
    const pickers = html`<div class="pickers" ?hidden=${!pickersShown}><slot name="toolbar" @slotchange=${this.onToolbarSlot}><mosaica-view-picker exportparts=${FORWARD['view-picker']}></mosaica-view-picker><mosaica-key-picker exportparts=${FORWARD['key-picker']}></mosaica-key-picker></slot></div>`;
    // The heading names what is shown: the dataset's title over the view's name, or the view's
    // name alone. With one view the name is text; with several it is the view choice.
    const shown = meta?.views.find((v) => v.id === s?.get('view').id) ?? (meta?.views.length === 1 ? meta.views[0] : undefined);
    // A name that is only the view's id says nothing to a reader.
    const viewName = shown && shown.displayName !== shown.id ? shown.displayName : '';
    const viewText = (cls: string) => (pickersShown || !viewName ? nothing : html`<span part="view-name" class=${cls}>${viewName}</span>`);
    const names = this.datasetTitle
      ? html`<div class="names"><span part="dataset-title" class="title">${this.datasetTitle}</span><div class="sub">${pickers}${viewText('')}</div></div>`
      : html`<div class="names">${pickers}${viewText('title')}</div>`;
    const named = pickersShown || viewName !== '' || this.datasetTitle !== '';
    const layersPanel = html`<slot name="layers"><mosaica-layer-picker exportparts=${FORWARD['layer-picker']}></mosaica-layer-picker></slot>`;
    const filters = html`<slot name="filters"><mosaica-filter-panel exportparts=${FORWARD['filter-panel']} pinned=${this.pinnedFilters || nothing} ?compact=${compact || narrow} .clusterLevel=${level}></mosaica-filter-panel></slot>`;
    const selectionPanel = this.has('selection') && region ? html`<slot name="selection"><mosaica-selection exportparts=${FORWARD.selection}></mosaica-selection></slot>` : nothing;
    const selectionCard = selectionPanel === nothing ? nothing : html`<div part="selection-card" class="card">${selectionPanel}</div>`;
    const head = html`<div class="head" ?hidden=${!named}>${names}</div>`;

    // The card beside the point, where the map holds a position for what is selected; else the
    // card stands in the top-right corner.
    const anchor = this.liveAnchor(showArtifact);
    const detailInColumn = this.has('detail') && hasDetail && anchor === null;
    const right = html`<div slot="top-right" class="right"><slot name="top-right"></slot>${selectionCard}${detailInColumn ? html`<div part="detail" class="card">${detail}</div>` : nothing}</div>`;

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
            ? html`<div part="layers-popover" id="layers-popover" class="card" role="dialog" aria-label="Layers and display" @pointerdown=${this.onPopoverPress}>${this.displaySection()}${layersPanel}</div>${this.choiceMenu()}`
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
          ? html`${this.displaySection()}${layersPanel}${this.choiceMenu()}`
          : this.sheet === 'detail'
            ? html`${selectionPanel}${detail}`
            : nothing;

    // The tooltip slot is forwarded only when the host supplied one: a slot assigned another slot
    // counts as filled even when that slot is empty, which would hide the map's own tooltip.
    return html`<div part="frame" class=${`${floating ? 'floating' : 'docked'}${compact ? ' compact' : ''}`}
      @mosaica-levelchange=${(e: CustomEvent<{level: number | null}>) => (this.level = e.detail.level)}
      @mosaica-artifactfit=${(e: CustomEvent<{id: string}>) => this.map?.fitTo(BigInt(e.detail.id))}
      @mosaica-viewfollow=${(e: CustomEvent<{view: string; x: number; y: number}>) => this.followItem(e.detail)}
      @mosaica-close=${(e: Event) => this.closeDetail(e)}>
      ${floating || narrow ? nothing : docked}
      <div class="stage">
        ${floating && !narrow ? panel : nothing}
        <mosaica-map
          exportparts=${FORWARD.map}
          colour-by=${this.colourBy || nothing}
          layers=${this.layers || nothing}
          tooltip-fields=${this.tooltipFields}
          title-field=${this.titleField || nothing}
          budget=${this.budget || nothing}
          .budgetMin=${this.budgetMin}
          .budgetMax=${this.budgetMax}
          .clusterBudget=${this.clusterBudget}
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
          .palette=${this.palette}
          .categoryPalette=${this.categoryPalette}
          .ramp=${this.ramp}
          .rampScale=${this.rampScale}
          .rampReverse=${this.rampReverse}
          .valueColours=${this.valueColours}
          .clusterColours=${this.clusterColours}
          @mosaica-viewchange=${() => this.onCamera()}
          @mosaica-pick=${() => this.requestUpdate()}
          @mosaica-miss=${() => this.onMiss()}
          @keydown=${this.onMapKey}
          @click=${() => this.requestUpdate()}
        >
          ${narrow ? nothing : layersButton}
          ${narrow ? nothing : right}
          ${narrow ? nothing : html`<div slot="bottom-right" class="in-map-strip"><slot name="status"><mosaica-status exportparts=${FORWARD.status} ?compact=${compact}></mosaica-status></slot></div>`}
          ${this.querySelector('[slot="tooltip"]') ? html`<slot name="tooltip" slot="tooltip"></slot>` : nothing}
        </mosaica-map>
        ${narrow ? nothing : this.callouts(this.has('detail') && hasDetail ? anchor : null, detail)}
      </div>
      ${this.sheet && sheetBody !== nothing
        ? html`<div part="sheet" id="sheet" role="dialog" aria-labelledby=${`tab-${this.sheet}`} tabindex="-1" @keydown=${this.onSheetKey}>${sheetBody}</div>`
        : nothing}
      <div part="strip-row"><mosaica-status exportparts=${FORWARD.status}></mosaica-status></div>
      <div part="tabs" role="tablist" aria-label="Explorer panels" @keydown=${this.onTabKey}>${tabs.map(tab)}</div>
      <mosaica-colour-editor exportparts=${FORWARD['colour-editor']} field=${s?.get('legend').colourBy ?? ''} .level=${level}></mosaica-colour-editor>
    </div>`;
  }

  /** The item or artifact card for what is selected, or the host's in the `detail` slot. */
  private liveCard(showArtifact: boolean, pin: TemplateResult | typeof nothing = nothing): TemplateResult {
    return html`<slot name="detail">${showArtifact
      ? html`<mosaica-artifact-card exportparts=${FORWARD['artifact-card']}>${pin}</mosaica-artifact-card>`
      : html`<mosaica-item-card exportparts=${FORWARD['item-card']} compact title-field=${this.titleField || nothing} subtitle-field=${this.subtitleField || nothing} .pick=${this.map?.lastPick ?? null}>${pin}</mosaica-item-card>`}</slot>`;
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

  /** What a card is called: its item's title field, else its `mosaica_id`; a cluster's name. */
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
      const row = a?.served.find((x) => x.mosaicaId === id) ?? a?.colourServed.find((x) => x.mosaicaId === id);
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
            ? html`<mosaica-item-card exportparts=${FORWARD['item-card']} compact .item=${p.item} title-field=${this.titleField || nothing} subtitle-field=${this.subtitleField || nothing}>${unpin}</mosaica-item-card>`
            : html`<mosaica-artifact-card exportparts=${FORWARD['artifact-card']} .artifact=${p.artifact}>${unpin}</mosaica-artifact-card>`;
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
    });
  }

  /**
   * Tab from the map itself goes to the card beside the point, the selection's where it shows, so
   * the card is the next stop after the map; from there Tab goes on in the page's order.
   */
  private onMapKey = (e: KeyboardEvent): void => {
    const from = e.composedPath()[0];
    if (e.key !== 'Tab' || e.shiftKey || !this.map || (from !== this.map && from !== this.map.focusTarget)) return;
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
   * The Points, Colour and Density sections of the Layers popover, over the layer picker. The Size
   * and Opacity sliders show what the map drew last while the setting is unset, and moving one
   * fixes it.
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
        ? nothing
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
            <div class="with-readout"><input id="density-strength" part="density-strength" class="slider" type="range" min="0.1" max="1" step="0.05" .value=${String(s.densityStrength)}
              style=${fill(s.densityStrength, 0.1, 1)} @input=${(e: Event) => change({densityStrength: number(e)})} /><span class="readout">${Math.round(s.densityStrength * 100)}%</span></div>
          </div>`;
    return html`<div part="display" class="display">
      <section class="sec">
        <div class="sec-head">
          <span class="hd" id="points-label">Points</span>
          <button part="points-toggle" class="switch small" type="button" role="switch" aria-checked=${s.points ? 'true' : 'false'} aria-label="Show points"
            @click=${() => change({points: !s.points})}><span class="knob"></span></button>
        </div>
        ${this.mostPoints(!s.points)}${this.mostClusters()}
        <div class="sliders">
          ${this.sizeControls(radius, !s.points)}
          <label for="point-opacity">Opacity</label>
          <input id="point-opacity" part="point-opacity" class="slider" type="range" min="0.1" max="1" step="0.05" .value=${String(opacity)} ?disabled=${!s.points}
            style=${fill(opacity, 0.1, 1)} aria-valuetext=${`${Math.round(opacity * 100)}%`} @input=${(e: Event) => change({pointOpacity: number(e)})} />
        </div>
      </section>
      ${this.colourSection()}
      <section class="sec">
        <div class="hd" id="density-label">Density</div>
        <div part="density-mode" class="modes" role="radiogroup" aria-labelledby="density-label">
          ${DENSITY_MODES.map(
            (m, i) => html`<button type="button" role="radio" data-mode=${m.mode} aria-checked=${m.mode === s.density ? 'true' : 'false'} tabindex=${i === modeAt ? '0' : '-1'}
              @click=${() => change({density: m.mode})}
              @keydown=${(e: KeyboardEvent) => radioKeys(e, DENSITY_MODES.length, i, (j) => change({density: DENSITY_MODES[j]!.mode}))}>${m.label}</button>`
          )}
        </div>
        ${densityControls}
      </section>
    </div>`;
  }

  /**
   * Most points: a slider from `budget-min` to `budget-max` on a log scale, on round numbers, with
   * the number it stands at. Dragging moves only the number; letting go sets the store's budget.
   */
  private mostPoints(disabled: boolean): TemplateResult {
    const s = this.resolvedStore;
    return this.mostSlider({
      id: 'most-points',
      label: 'Most points',
      unit: 'points',
      range: [this.budgetMin, this.budgetMax],
      shown: this.pointsDragged ?? s?.budget ?? 250_000,
      disabled: disabled || !s,
      drag: (n) => (this.pointsDragged = n),
      set: (n) => this.setBudget(n)
    });
  }

  /**
   * Most clusters, while a `nested` or `dag` layer is drawn: a slider from `cluster-budget-min` to
   * `cluster-budget-max` as Most points is. Letting go sets the most clusters such a layer is cut to.
   */
  private mostClusters(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return nothing;
    const treed = s.get('artifacts').layers.some((name) => {
      const kind = meta.layers.find((l) => l.name === name)?.hierarchy.kind;
      return kind === 'nested' || kind === 'dag';
    });
    if (!treed) return nothing;
    return this.mostSlider({
      id: 'most-clusters',
      label: 'Most clusters',
      unit: 'clusters',
      range: [this.clusterBudgetMin, this.clusterBudgetMax],
      shown: this.clustersDragged ?? s.clusterBudget ?? (this.clusterBudget || this.clusterBudgetMax),
      disabled: false,
      drag: (n) => (this.clustersDragged = n),
      set: (n) => this.setClusterBudget(n)
    });
  }

  /** A log-scale slider over `range` on round numbers, with the number it stands at. */
  private mostSlider(o: {
    id: 'most-points' | 'most-clusters';
    label: string;
    unit: string;
    range: [number, number];
    shown: number;
    disabled: boolean;
    drag: (n: number) => void;
    set: (n: number) => void;
  }): TemplateResult {
    const min = Math.max(1, o.range[0]);
    const max = Math.max(min * 10, o.range[1]);
    const span = Math.log(max / min);
    const at = (n: number) => Math.min(1, Math.max(0, Math.log(n / min) / span));
    const of = (t: number) => Math.min(max, Math.max(min, roundPoints(min * Math.exp(t * span))));
    const t = at(o.shown);
    // A tick at each power of ten between the ends, where it leaves room for the last end's label.
    const ticks: number[] = [];
    for (let p = 10 ** Math.ceil(Math.log10(min) + 1e-9); p < max; p *= 10) if (at(p) > 0.08 && at(p) < 0.85) ticks.push(p);
    const read = (e: Event) => of(Number((e.target as HTMLInputElement).value) / 1000);
    return html`<div class="most">
      <div class="most-top"><label for=${o.id}>${o.label}</label><span part=${o.id === 'most-points' ? 'most-points-value' : 'most-clusters-value'} class="most-value">${pointsText(o.shown)}</span></div>
      <input id=${o.id} part=${o.id === 'most-points' ? 'most-points' : 'most-clusters'} class="slider" type="range" min="0" max="1000" step="1" .value=${String(Math.round(t * 1000))} ?disabled=${o.disabled}
        style=${`--fill:${(t * 100).toFixed(1)}%`} aria-valuetext=${`${pointsText(o.shown)} ${o.unit}`}
        @input=${(e: Event) => o.drag(read(e))}
        @change=${(e: Event) => o.set(read(e))} />
      <div class="ticks" aria-hidden="true">
        <span style="left:0">${pointsText(min)}</span>
        ${ticks.map((p) => html`<span class="mid" style=${`left:${(at(p) * 100).toFixed(1)}%`}>${pointsText(p)}</span>`)}
        <span style="right:0">${pointsText(max)}</span>
      </div>
    </div>`;
  }

  /** Set the store's budget from Most points, and report it. */
  private setBudget(budget: number): void {
    this.pointsDragged = null;
    const s = this.resolvedStore;
    if (!s || budget === s.budget) return;
    s.setBudget(budget);
    emit(this, 'mosaica-budgetchange', {budget});
    this.requestUpdate();
  }

  /** Set the most clusters a treed layer is cut to from Most clusters, and report it. */
  private setClusterBudget(budget: number): void {
    this.clustersDragged = null;
    const s = this.resolvedStore;
    if (!s || budget === s.clusterBudget) return;
    s.setClusterBudget(budget);
    this.clusterBudget = budget;
    emit(this, 'mosaica-clusterbudgetchange', {budget});
  }

  /**
   * The Colour section: Colour by, then the palette for a category, or the ramp, its scale and
   * Reverse for a number or a date, unless `hide-palettes` is set.
   */
  private colourSection(): TemplateResult {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    const colourBy = s?.get('legend').colourBy ?? null;
    const colouring = colouringOf(s);
    const current = this.colourOptions().find((o) => o.checked);
    const column = meta?.declaredScalars.find((c) => c.name === colourBy && c.render) ?? null;
    const scales: RampScale[] = ['linear', 'log'];
    const scaleAt = scales.indexOf(colouring.scale);
    const clusters = colourBy?.startsWith(CLUSTER_PREFIX) ?? false;
    const palette = s && clusters ? PALETTES[s.get('artifacts').palette] : CATEGORY_PALETTES[colouring.palette];
    const choice = (menu: Menu, label: string, body: TemplateResult, stretch = false) =>
      html`<button part=${({size: 'size-by', colour: 'colour-by', palette: 'palette', ramp: 'ramp'} as const)[menu]} class=${`ramp-choice${stretch ? ' stretch' : ''}`} type="button" aria-haspopup="menu" aria-expanded=${this.menu === menu ? 'true' : 'false'}
        aria-label=${label} ?disabled=${!meta} @click=${() => (this.menu = this.menu === menu ? null : menu)} @keydown=${this.onMenuButtonKey(menu)}>${body}${icon('chev', 12, 1.4)}</button>`;
    const byName = current?.title ?? 'None';
    const palettes =
      (column?.category || clusters) && !this.hidePalettes
        ? html`<span>Palette</span>${choice('palette', `Palette: ${palette.title}`, html`<span class="strip">${palette.colours.slice(0, 10).map((c) => html`<span style=${`background:${hexOf(c)}`}></span>`)}</span><span class="t">${palette.title}</span>`, true)}`
        : nothing;
    const ramps =
      column && !column.category && !this.hidePalettes
        ? html`<span>Ramp</span>${choice('ramp', `Ramp: ${RAMPS[colouring.ramp].title}`, html`<span class="bar wide" style=${`background:${rampGradient(colouring.ramp, colouring.reverse)}`}></span><span class="t">${RAMPS[colouring.ramp].title}</span>`, true)}
            <span id="ramp-scale-label">Scale</span>
            <div class="scale-row">
              <div part="ramp-scale" class="seg" role="radiogroup" aria-labelledby="ramp-scale-label">
                ${scales.map(
                  (x, i) => html`<button type="button" role="radio" data-scale=${x} aria-checked=${x === colouring.scale ? 'true' : 'false'} tabindex=${i === Math.max(0, scaleAt) ? '0' : '-1'}
                    @click=${() => this.choosePalette({scale: x})}
                    @keydown=${(e: KeyboardEvent) => radioKeys(e, scales.length, i, (j) => this.choosePalette({scale: scales[j]!}))}>${x === 'linear' ? 'Linear' : 'Log'}</button>`
                )}
              </div>
              <button part="ramp-reverse" class="toggle" type="button" aria-pressed=${colouring.reverse ? 'true' : 'false'} @click=${() => this.choosePalette({reverse: !colouring.reverse})}>Reverse</button>
            </div>`
        : nothing;
    // A category's values and a layer's clusters have colours of their own to edit.
    const editable = Boolean(column?.category) || clusters;
    return html`<section class="sec">
      <div class="hd">Colour</div>
      <div class="sliders">
        <span>Colour by</span>${choice('colour', `Colour by: ${byName}`, html`<span class="t">${byName}</span>`)}
        ${palettes}${ramps}
      </div>
      <button part="edit-colours" class="btn small edit-colours" type="button" aria-haspopup="dialog" ?disabled=${!editable || !meta} @click=${() => this.renderRoot.querySelector<MosaicaColourEditor>('mosaica-colour-editor')?.show()}>Edit colours…</button>
    </section>`;
  }

  /**
   * What Colour by offers: None, every layer that can colour (a levelled layer once per level, by
   * its levels' titles), and the rendered category and number columns.
   */
  private colourOptions(): {value: string; title: string; kind: string; checked: boolean}[] {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    const colourBy = s?.get('legend').colourBy ?? null;
    if (!meta) return [{value: '', title: 'None', kind: '', checked: true}];
    const layers = colourLayers(meta.layers).flatMap((decl) => {
      const value = `${CLUSTER_PREFIX}${decl.name}`;
      if (decl.levels.length <= 1) return [{value, title: decl.title || decl.name, kind: 'Clusters', checked: colourBy === value}];
      const drawn = this.level ?? decl.levels.at(-1)!.level;
      return decl.levels.map((lv) => ({value: `${value}@${lv.level}`, title: lv.title || `${decl.title || decl.name}, level ${lv.level}`, kind: 'Clusters', checked: colourBy === value && drawn === lv.level}));
    });
    const columns = meta.declaredScalars.filter((c) => c.render).map((c) => ({value: c.name, title: columnCaption(c.name), kind: c.category ? 'Category' : c.arrowType === 'timestamp_us' ? 'Date' : 'Number', checked: colourBy === c.name}));
    return [{value: '', title: 'None', kind: '', checked: colourBy === null}, ...layers, ...columns];
  }

  /** Colour by `value`: a column, `cluster:<layer>`, or `cluster:<layer>@<level>` for one level of a layer. */
  private chooseColour(value: string): void {
    const s = this.resolvedStore;
    if (!s) return;
    const at = value.lastIndexOf('@');
    const colourBy = value === '' ? null : at > 0 ? value.slice(0, at) : value;
    s.setColourBy(colourBy);
    emit(this, 'mosaica-colourchange', {colourBy});
    if (at > 0) {
      this.level = Number(value.slice(at + 1));
      emit(this, 'mosaica-levelchange', {level: this.level});
    }
  }

  /** Colour a layer's clusters from `palette`, and report it. */
  private chooseClusterPalette(palette: PaletteName): void {
    const s = this.resolvedStore;
    if (!s) return;
    s.setPalette(palette);
    this.palette = palette;
    emit(this, 'mosaica-clusterpalettechange', {palette});
  }

  /** Change the palette, the ramp, its scale or its direction, and report all four. */
  private choosePalette(patch: Partial<Pick<Colouring, 'palette' | 'ramp' | 'scale' | 'reverse'>>): void {
    const s = this.resolvedStore;
    if (!s) return;
    setColouring(s, patch);
    const {palette, ramp, scale, reverse} = colouringOf(s);
    emit(this, 'mosaica-palettechange', {palette, ramp, scale, reverse});
    this.requestUpdate();
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
      <button part="size-by" class="ramp-choice" type="button" aria-haspopup="menu" aria-expanded=${this.menu === 'size' ? 'true' : 'false'} aria-label=${`Size by: ${sizeBy === null ? 'None' : columnCaption(sizeBy)}`}
        ?disabled=${disabled || (sizeBy === null && this.sizeColumns.length === 0)} title=${this.sizeColumns.length === 0 ? 'No number column to size by' : nothing}
        @click=${() => (this.menu = this.menu === 'size' ? null : 'size')} @keydown=${this.onMenuButtonKey('size')}><span class="t">${sizeBy === null ? 'None' : columnCaption(sizeBy)}</span>${icon('chev', 12, 1.4)}</button>`;
    if (sizeBy === null) {
      if (this.hideSize) return choice;
      return html`${choice}<label for="point-size">Size</label>
        <input id="point-size" part="point-size" class="slider" type="range" min=${min} max=${max} step=${step} .value=${String(radius)} ?disabled=${disabled}
          style=${fill(radius, min, max)} aria-valuetext=${`${px(radius)} px`} @input=${(e: Event) => this.changeDisplay({radius: number(e)})} />`;
    }
    const at = SIZE_SCALES.findIndex((x) => x.scale === sizing.scale);
    // Moving one end past the other takes the other with it.
    return html`${choice}<span id="size-range-label">Range</span>
      <div class="range" role="group" aria-labelledby="size-range-label">
        <input part="size-min" class="slider" type="range" min=${min} max=${max} step=${step} aria-label="Smallest size" .value=${String(sizing.min)} ?disabled=${disabled}
          style=${fill(sizing.min, min, max)} @input=${(e: Event) => this.changeSize({min: number(e), max: Math.max(number(e), sizing.max)})} />
        <span part="size-range" class="readout">${px(sizing.min)} – ${px(sizing.max)} px</span>
        <input part="size-max" class="slider" type="range" min=${min} max=${max} step=${step} aria-label="Largest size" .value=${String(sizing.max)} ?disabled=${disabled}
          style=${fill(sizing.max, min, max)} @input=${(e: Event) => this.changeSize({max: number(e), min: Math.min(number(e), sizing.min)})} />
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

  /**
   * The entries of `menu`, each with what it shows beside its title, and the one chosen. A palette
   * is a row: its colours, its title and a line under it.
   */
  private menuEntries(menu: Menu): {title: string; entries: {value: string; title: string; kind: string; swatch?: TemplateResult; line?: string}[]; checked: string; choose: (value: string) => void} {
    const colouring = colouringOf(this.resolvedStore);
    switch (menu) {
      case 'size':
        return {
          title: 'Size by',
          entries: [{value: '', title: 'None', kind: ''}, ...this.sizeColumns.map((c) => ({value: c, title: columnCaption(c), kind: 'Number'}))],
          checked: this.sizedBy ?? '',
          choose: (value) => this.changeSize({sizeBy: value === '' ? null : value})
        };
      case 'colour': {
        const options = this.colourOptions();
        return {
          title: 'Colour by',
          entries: options,
          checked: options.find((o) => o.checked)?.value ?? '',
          choose: (value) => this.chooseColour(value)
        };
      }
      case 'palette': {
        const strip = (hexes: readonly string[]) => html`<span class="swatches">${hexes.map((c) => html`<span style=${`background:${c}`}></span>`)}</span>`;
        const s = this.resolvedStore;
        if (s?.get('legend').colourBy?.startsWith(CLUSTER_PREFIX)) {
          return {
            title: 'Palette',
            entries: (Object.keys(PALETTES) as PaletteName[]).map((name) => ({value: name, title: PALETTES[name].title, kind: '', line: PALETTES[name].description, swatch: strip(PALETTES[name].colours.map(hexOf))})),
            checked: s.get('artifacts').palette,
            choose: (value) => this.chooseClusterPalette(value as PaletteName)
          };
        }
        return {
          title: 'Palette',
          entries: (Object.keys(CATEGORY_PALETTES) as CategoryPaletteName[]).map((name) => {
            const p = CATEGORY_PALETTES[name];
            return {value: name, title: p.title, kind: '', line: `${p.colours.length} colours${p.colourBlindSafe ? ' · colour-blind safe' : ''}`, swatch: strip(p.colours.map(hexOf))};
          }),
          checked: colouring.palette,
          choose: (value) => this.choosePalette({palette: value as CategoryPaletteName})
        };
      }
      case 'ramp':
        return {
          title: 'Ramp',
          entries: (Object.keys(RAMPS) as RampName[]).map((name) => ({value: name, title: RAMPS[name].title, kind: '', swatch: html`<span class="bar" style=${`background:${rampGradient(name, colouring.reverse)}`}></span>`})),
          checked: colouring.ramp,
          choose: (value) => this.choosePalette({ramp: value as RampName})
        };
    }
  }

  /** The menu open beside the Layers popover, in the top layer: Size by, Colour by, Palette or Ramp. */
  private choiceMenu(): TemplateResult | typeof nothing {
    const menu = this.menu;
    if (!menu) return nothing;
    const {title, entries, checked, choose} = this.menuEntries(menu);
    const pick = (value: string) => {
      this.closeMenu(true);
      choose(value);
    };
    // One entry is in the tab order, the one focused last, as in a radio group.
    const keys = (e: KeyboardEvent, i: number) => {
      const items = Array.from(this.renderRoot.querySelectorAll<HTMLElement>(`[part~="${menu}-option"]`));
      const next = {ArrowDown: (i + 1) % items.length, ArrowUp: (i - 1 + items.length) % items.length, Home: 0, End: items.length - 1}[e.key];
      if (e.key === 'Escape') {
        e.stopPropagation();
        this.closeMenu(true);
        return;
      }
      if (next === undefined) return;
      e.preventDefault();
      items.forEach((item, j) => (item.tabIndex = j === next ? 0 : -1));
      items[next]?.focus();
    };
    const at = Math.max(0, entries.findIndex((o) => o.value === checked));
    const row = (o: (typeof entries)[number]) =>
      o.line === undefined
        ? html`<span class="lead">${o.swatch ?? nothing}<span>${o.title}</span></span><span class="kind">${o.kind}</span>`
        : html`${o.swatch ?? nothing}<span class="named"><span class="t">${o.title}</span><span class="line">${o.line}</span></span>`;
    return html`<div part=${({size: 'size-menu', colour: 'colour-menu', palette: 'palette-menu', ramp: 'ramp-menu'} as const)[menu]} class=${`size-menu${menu === 'ramp' ? ' wide' : menu === 'palette' ? ' palettes' : ''}`} popover="manual" role="menu" aria-labelledby="menu-label" @focusout=${this.onMenuFocusOut}>
      <div class="hd" id="menu-label">${title}</div>
      ${entries.map(
        (o, i) => html`<button part=${({size: 'size-option', colour: 'colour-option', palette: 'palette-option', ramp: 'ramp-option'} as const)[menu]} type="button" role="menuitemradio" data-value=${o.value} aria-checked=${o.value === checked ? 'true' : 'false'}
          tabindex=${i === at ? '0' : '-1'} @click=${() => pick(o.value)} @keydown=${(e: KeyboardEvent) => keys(e, i)}>${row(o)}</button>`
      )}
    </div>`;
  }

  /** Close the open menu, putting focus back on its button with `refocus`. */
  private closeMenu(refocus: boolean): void {
    const menu = this.menu;
    this.menu = null;
    if (refocus && menu) void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>(`[part="${MENU_BUTTON[menu]}"]`)?.focus());
  }

  /** The arrow keys on a menu's button open the menu, as a menu button's do. */
  private onMenuButtonKey(menu: Menu): (e: KeyboardEvent) => void {
    return (e) => {
      if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
      e.preventDefault();
      this.menu = menu;
    };
  }

  /** Focus leaving the open menu, by Tab or otherwise, closes it; Tab goes on from where focus went. */
  private onMenuFocusOut = (e: FocusEvent): void => {
    const to = e.relatedTarget as Node | null;
    if (to && (e.currentTarget as HTMLElement).contains(to)) return;
    this.closeMenu(false);
  };

  /** A press in the Layers popover outside the open menu's button closes the menu. */
  private onPopoverPress = (e: PointerEvent): void => {
    if (!this.menu) return;
    const button = this.renderRoot.querySelector(`[part="${MENU_BUTTON[this.menu]}"]`);
    if (button && !e.composedPath().includes(button)) this.menu = null;
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
    emit(this, 'mosaica-sizechange', {
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
    emit(this, 'mosaica-displaychange', this.display);
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
    this.placeMenu(changed.has('menu') && this.menu !== null);
    this.followLayout();
    if (!changed.has('sheet')) return;
    const before = changed.get('sheet');
    if (this.sheet) this.renderRoot.querySelector<HTMLElement>('[part="sheet"]')?.focus();
    else if (before) this.renderRoot.querySelector<HTMLElement>(`[role="tab"][data-sheet="${before}"]`)?.focus();
  }

  /**
   * Show the open menu in the top layer, so the popover's scrolling does not clip it, beside the
   * popover or the sheet and level with its button; focus goes to the entry chosen as it opens.
   */
  private placeMenu(opened: boolean): void {
    const menu = this.menu ? this.renderRoot.querySelector<HTMLElement>(`[part="${this.menu}-menu"]`) : null;
    const button = this.menu ? this.renderRoot.querySelector<HTMLElement>(`[part="${MENU_BUTTON[this.menu]}"]`) : null;
    const beside = this.renderRoot.querySelector<HTMLElement>('[part="layers-popover"], [part="sheet"]');
    if (!menu || !button || !beside) return;
    if (typeof menu.showPopover === 'function' && !menu.matches(':popover-open')) {
      try {
        menu.showPopover();
      } catch {
        // Shown already; the menu is in the page either way.
      }
    }
    const b = button.getBoundingClientRect();
    const p = beside.getBoundingClientRect();
    const width = menu.offsetWidth || 220;
    const height = menu.offsetHeight || 0;
    const vw = typeof innerWidth === 'number' ? innerWidth : 1024;
    const vh = typeof innerHeight === 'number' ? innerHeight : 768;
    const right = p.right + 16;
    // Where there is no room either side, as in a sheet, the menu opens under its button.
    const left = right + width <= vw - 8 ? right : p.left - width - 16 >= 8 ? p.left - width - 16 : Math.max(8, Math.min(b.left, vw - width - 8));
    const under = right + width > vw - 8 && p.left - width - 16 < 8;
    menu.style.left = `${Math.round(left)}px`;
    menu.style.top = `${Math.round(Math.max(8, Math.min(under ? b.bottom + 4 : b.top - 32, vh - height - 8)))}px`;
    if (opened) (menu.querySelector<HTMLElement>('[aria-checked="true"]') ?? menu.querySelector<HTMLElement>('button'))?.focus();
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
defineOnce('mosaica-explorer', MosaicaExplorer);

declare global {
  interface HTMLElementTagNameMap {
    'mosaica-explorer': MosaicaExplorer;
  }
}
