import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {
  CLUSTER_PREFIX,
  artifactName,
  colourLayers,
  composeFilters,
  isPopulated,
  withMember,
  withoutMember,
  type AggregateEntry,
  type AggregateSpec,
  type AggregateTable,
  type BrowseRow,
  type ClauseVerb,
  type ColumnDraft,
  type FieldSummary,
  type FilterDraft,
  type Layer,
  type Meta,
  type Store
} from '@tesseradb/client';
import {NEUTRAL} from '@tesseradb/client/internal';
import {CATEGORY_PALETTES, RAMPS, type Colouring} from '@tesseradb/deck';
import {UNMAPPED, colourOfFraction, colourOfRank, css as rgb, fractionOf, hexOf, lighter, rgbOfHex} from '@tesseradb/deck/internal';
import {HeldAggregate, artifactGroupings, countedByLevel, countsByKey, listedGroups} from './aggregate.js';
import {TesseraElement, UNNAMED, columnCaption, dateRangeText, emit, idString, keyTitle, shortCount, shortDateText} from './base.js';
import {colouringOf, setColouring, watchChoices, withValueColour} from './colouring.js';
import {attachContextRoot, defineOnce} from './define.js';
import {hsvOf, rgbOfHsv, type Hsv} from './hsv.js';
import {icon} from './icons.js';
import {exportparts} from './parts.js';
import {chrome, tokens} from './tokens.js';
import './filter.js';
import './cluster-filter.js';

/** The rows a category or cluster card lists, and the rows "N more" opens it to. */
const ROWS = 5;
const MORE_ROWS = 20;
/** The narrowest a cluster's bar is drawn where it is not empty, in pixels. */
const MIN_BAR_PX = 3;
/** The values the counts over everything matching are asked for, so the rows in view find theirs. */
const MATCH_TOP = 100;
/** A histogram's bins, and the items its counts are taken from beyond which they are scaled. */
const BINS = 20;
const SAMPLE = 100_000;
/** The histogram's height in pixels. */
const PLOT_HEIGHT = 56;
/** The bars of a folded card's small chart. */
const SPARK_BARS = 8;
/** The width of the colour picker's hue knob, which its travel along the bar allows for. */
const HUE_KNOB = 12;
/** How far each lighter colour in the colour picker is taken towards white. */
const LIGHTER = 0.45;

/** A count as a row shows it: whole below a million, else shortened, `25.2M`. */
export function countText(n: number): string {
  return Math.abs(n) < 1e6 ? Math.round(n).toLocaleString('en-GB') : shortCount(n);
}

/**
 * A number as an axis prints it: in exponent form from a million up and below a thousandth, else
 * to two decimal places at 1 or more and three significant figures below 1. With `whole`, rounded
 * to a whole number first, for an integer column.
 */
function formatNumber(n: number, whole: boolean): string {
  const v = whole ? Math.round(n) : n;
  const size = Math.abs(v);
  if (size >= 1e6 || (v !== 0 && size < 1e-3)) return v.toExponential(2);
  return v.toLocaleString('en-GB', size >= 1 ? {maximumFractionDigits: 2} : {maximumSignificantDigits: 3});
}

/** What a card draws: a category's or a layer's top values, a histogram, or a search box. */
type Kind = 'category' | 'cluster' | 'histogram' | 'search';

/** One bin of a histogram, its edges in the column's units (microseconds on a timestamp). */
type Bin = {lower: number; upper: number; count: number};

/** One row of a category or cluster card. */
type Row = {key: string; name: string; path: string; sub: number | null; all: number | null; swatch: string | null};

/** A range dragged across the histogram, by bin, `from` where the drag began. */
type Brush = {from: number; to: number; open: boolean};

/** The value whose colour the picker is changing. */
type Picking = {key: string; title: string};

/** The listed bins of a histogram table, in order, with the edges in the column's units. */
function binsOf(table: AggregateTable | undefined, timestamp: boolean): Bin[] {
  if (!table) return [];
  const rows = table.rows;
  const group = rows.getChild('group');
  const lower = rows.getChild('lower');
  const upper = rows.getChild('upper');
  const count = rows.getChild('count');
  if (!group || !lower || !upper || !count) return [];
  // Arrow reads a timestamp in milliseconds; a clause on one is in microseconds.
  const unit = timestamp ? 1000 : 1;
  const out: Bin[] = [];
  for (let i = 0; i < rows.numRows; i++) {
    if (group.get(i) !== 'listed') continue;
    const lo = lower.get(i) as number | bigint | Date | null;
    const hi = upper.get(i) as number | bigint | Date | null;
    if (lo === null || hi === null) continue;
    out.push({lower: Number(lo instanceof Date ? lo.getTime() : lo) * unit, upper: Number(hi instanceof Date ? hi.getTime() : hi) * unit, count: Number(count.get(i) as bigint | number)});
  }
  return out;
}

/**
 * One field as a card in the field column: a category's commonest values, a layer's largest
 * clusters, a number's or a date's histogram, or a text field's search box, under the field's name.
 * `field` is a column `meta.filterOperands` lists, or `cluster:<layer>` for a layer whose clusters
 * can be filtered by.
 *
 * The counts are taken over two sets. The subject is what is in view: the items in the camera's box
 * that the filters admit, or the highlighted ones while a highlight is set. The whole match is every
 * item the filters admit. Both leave out the card's own clause in the filter position, so a value
 * its clause excludes keeps its count. The card keeps three aggregates registered with the store
 * while it is drawn (`Store.setAggregate`): the subject's counts (`subject: 'view'` with
 * `highlighted`), the whole match's, and for a number or a date its figures over everything the
 * viewer may see (`subject: 'visible'`). A folded card asks for the subject's alone.
 *
 * A category or cluster card lists five values, those with most items in the subject first and any
 * its own clauses name, with "N more" opening it to twenty. Each row has its name (a cluster's
 * parents in grey after it), a solid bar for its share of the subject and a pale bar for its share
 * of the whole match, each a share of its own total, and the two counts. Under a filter on the
 * field, the values it leaves out are greyed and show only how many items they would add, and the
 * shares are of what the filter keeps. Hovering or focusing a row shows Highlight and Filter, which
 * put the value in or out of the field's clause in that position. Over the rows is the field's
 * search box, the typeahead of `<tessera-filter>` or `<tessera-cluster-filter>`, whose choice joins
 * the filter. A levelled layer's card has a Level choice in its heading, the deepest level by
 * default, which fires `tessera-levelchange`. A cluster card ranks a `nested` or `dag` layer's
 * clusters at the cut the map draws (`cut: 'drawn'`), and a flat or levelled layer's at its level.
 * A cluster is named as the map names it, and otherwise from the layer's browse pages, which also
 * name its path.
 *
 * A number or date card draws about twenty bins over the values the viewer may see, the pale bar
 * the whole match and the solid one the subject, each bin's height its share of its own set, scaled
 * so the tallest share fills the plot. The counts are taken from a sample of about 100,000 items
 * where a set is larger, and an estimate is marked "≈". Dragging across the bins, or the arrow keys
 * with Shift on the focused plot and then Enter, opens a box naming the range and how many items
 * match in it, with Filter and Highlight, which set the field's range in that position. A range set
 * on the field is outlined on the plot. The heading gives the field's figures: how many items hold
 * a value and their mean, or their range while folded.
 *
 * A text or keyword field is its `<tessera-filter>` search box.
 *
 * The paint button colours the map by the field, or by nothing where it already does
 * (`Store.setColourBy`), and is pressed while it does. Then a category or cluster card shows each
 * value's colour beside it, and a number or date card the ramp under its plot. A category value's
 * colour is a button that opens a colour picker: the palette's colours and a lighter row, a custom
 * area with a hue bar and a hex field, and Reset, which gives the value its palette colour back. A
 * choice applies at once and fires `tessera-valuecolour`.
 *
 * `folded` draws the heading alone, with a small chart of the subject's counts.
 *
 * @summary One field's counts in view and overall, with its filter and highlight.
 * @tagname tessera-field-card
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A row's
 *   Highlight or Filter, or the range box, changed the field's clause.
 * @fires {CustomEvent<TesseraEventDetails['tessera-clausechange']>} tessera-clausechange - A cluster
 *   row's Highlight or Filter put a `member_of` clause on or took it off.
 * @fires {CustomEvent<TesseraEventDetails['tessera-colourchange']>} tessera-colourchange - The paint
 *   button changed what the map is coloured by.
 * @fires {CustomEvent<TesseraEventDetails['tessera-levelchange']>} tessera-levelchange - The Level
 *   choice of a levelled layer's card changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-valuecolour']>} tessera-valuecolour - A value's
 *   colour was chosen or reset in the colour picker.
 * @fires {CustomEvent<TesseraEventDetails['tessera-fold']>} tessera-fold - The fold button folded or
 *   opened the card.
 * @csspart head - The heading: the name, what it notes, and the buttons.
 * @csspart title - The field's name.
 * @csspart sub - What the heading notes: the field's figures, or the Level choice.
 * @csspart level-select - The Level choice, on a levelled layer's card.
 * @csspart spark - The small chart of a folded card.
 * @csspart paint - The paint button, with `aria-pressed`.
 * @csspart fold - The fold button, with `aria-expanded`.
 * @csspart rows - A category or cluster card's rows.
 * @csspart row - One row, with `data-key` and `data-state`: `in` for a value the field's filter
 *   keeps, `out` for one it leaves out, `lit` for one its highlight lights, else empty.
 * @csspart swatch - A row's colour, while the map is coloured by the field; a button for a category
 *   value.
 * @csspart name - A row's name, with `data-unnamed` for a cluster that has none.
 * @csspart path - A cluster row's parents.
 * @csspart bar-match - A row's or a bin's pale bar, its share of the whole match.
 * @csspart bar-subject - A row's or a bin's solid bar, its share of the subject.
 * @csspart count - A row's counts.
 * @csspart verbs - A row's Highlight and Filter buttons.
 * @csspart highlight - A row's Highlight button, with `aria-pressed`.
 * @csspart filter - A row's Filter button, with `aria-pressed`.
 * @csspart more - The "N more" button.
 * @csspart plot - The histogram, a focusable group.
 * @csspart bin - One bin, with `data-index`.
 * @csspart band - An outlined range on the histogram, with `data-verb` (`filter`, `highlight`, or
 *   `brush` while one is being chosen).
 * @csspart axis - The values under the histogram.
 * @csspart ramp - The colour ramp under the histogram, while the map is coloured by the field.
 * @csspart brush - The box a dragged range opens.
 * @csspart brush-filter - Its Filter button.
 * @csspart brush-highlight - Its Highlight button.
 * @csspart colour-popover - The colour picker, while it is open.
 * @csspart choice - A colour in the picker's palette rows, with `aria-pressed`.
 * @csspart sv - The picker's saturation and brightness area, a slider in two directions.
 * @csspart hue - The picker's hue bar, a slider.
 * @csspart hex - The picker's hex field.
 * @csspart reset - The picker's Reset button.
 * @csspart filter-<part> - A part of the inner `<tessera-filter>`.
 * @csspart cluster-filter-<part> - A part of the inner `<tessera-cluster-filter>`.
 */
export class TesseraFieldCard extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
        padding: 10px var(--_tessera-panel-inline, 14px) 12px;
        border-bottom: 1px solid var(--_tessera-line-2);
      }
      :host([folded]) {
        padding-top: 6px;
        padding-bottom: 6px;
      }
      [part='head'] {
        display: flex;
        align-items: center;
        gap: 8px;
        min-height: 24px;
      }
      /* Not a section heading, as the shared rule for a title part draws one. */
      [part='title'] {
        display: block;
        flex: none;
        margin: 0;
        font-size: 13px;
        font-weight: 600;
        letter-spacing: 0;
        text-transform: none;
        color: var(--_tessera-ink);
        white-space: nowrap;
      }
      [part='sub'] {
        flex: 1 1 auto;
        min-width: 0;
        font-size: 12px;
        color: var(--_tessera-ink-3);
        white-space: nowrap;
        overflow: hidden;
        text-overflow: ellipsis;
      }
      [part='sub'].choice {
        flex: 0 1 auto;
        margin-right: auto;
      }
      [part='sub'] select {
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
      [part='spark'] {
        flex: none;
        margin-left: auto;
        display: flex;
        align-items: flex-end;
        gap: 1px;
        width: 60px;
        height: 14px;
      }
      [part='spark'] span {
        flex: 1 1 0;
        min-height: 1px;
        border-radius: 1px 1px 0 0;
        background: var(--_tessera-bar-match);
      }
      :host([compact]) [part='spark'] {
        width: 72px;
      }
      /* Folded in the compact layout, a card is its name, a drop where it colours the map, its
         chart and its chevron. */
      :host([compact][folded]) [part='paint'],
      :host([compact][folded]) [part='sub'] {
        display: none;
      }
      :host([compact][folded]) [part='head'] {
        gap: 6px;
      }
      :host([compact][folded]) [part='fold'] {
        width: 16px;
      }
      :host([compact][folded]) [part='title'] {
        flex: 0 1 auto;
        min-width: 0;
        overflow: hidden;
        text-overflow: ellipsis;
      }
      .painted {
        display: inline-flex;
        flex: none;
        margin-left: -2px;
      }
      .painted path {
        fill: currentColor;
      }
      :host([compact]) [part='spark'] span {
        background: var(--_tessera-bar);
      }
      [part='paint'],
      [part='fold'] {
        flex: none;
        width: 22px;
        height: 22px;
        display: grid;
        place-items: center;
        border-radius: 5px;
        color: var(--_tessera-ink-2);
      }
      [part='paint'] {
        border: 1px solid var(--_tessera-line);
        background: var(--_tessera-surface);
        color: color-mix(in srgb, var(--_tessera-ink) 82%, var(--_tessera-surface));
      }
      [part='paint'][aria-pressed='true'] {
        border-color: var(--_tessera-accent);
        background: var(--_tessera-accent);
        color: var(--_tessera-accent-ink);
      }
      [part='fold']:hover,
      [part='paint'][aria-pressed='false']:hover {
        background: var(--_tessera-surface-2);
      }
      .body {
        margin-top: 6px;
        display: flex;
        flex-direction: column;
        gap: 2px;
      }
      .search {
        display: block;
        margin-bottom: 4px;
        --_tessera-input-height: 28px;
      }
      [part~='row'] {
        position: relative;
        display: grid;
        grid-template-columns: 12px minmax(0, 1fr) auto;
        align-items: center;
        column-gap: 8px;
        padding: 4px 6px;
        margin: 0 -6px;
        border-radius: 6px;
      }
      [part~='row']:hover,
      [part~='row']:focus-within,
      [part~='row'][data-shown] {
        background: var(--_tessera-surface-2);
      }
      [part~='row'][data-state='out'] {
        opacity: 0.55;
      }
      [part~='row'][data-state='in'] [part='name'] {
        font-weight: 600;
      }
      [part~='row'][data-state='lit'] [part='name'] {
        font-weight: 600;
        color: var(--_tessera-highlight);
      }
      .mark {
        width: 12px;
        display: grid;
        place-items: center;
      }
      [part='swatch'] {
        width: 10px;
        height: 10px;
        border-radius: 2px;
        background: var(--c);
      }
      button[part='swatch'] {
        cursor: pointer;
      }
      .label {
        display: flex;
        flex-direction: column;
        gap: 3px;
        min-width: 0;
      }
      .names {
        display: flex;
        align-items: baseline;
        gap: 6px;
        min-width: 0;
        white-space: nowrap;
      }
      [part='name'] {
        flex: 0 1 auto;
        overflow: hidden;
        text-overflow: ellipsis;
      }
      [part='path'] {
        flex: 1 1 0;
        min-width: 0;
        overflow: hidden;
        text-overflow: ellipsis;
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
      .bars {
        position: relative;
        height: 5px;
      }
      .bars span {
        position: absolute;
        left: 0;
        top: 0;
        height: 5px;
        border-radius: 2px;
      }
      [part='bar-match'] {
        background: var(--_tessera-bar-match);
      }
      [part='bar-subject'] {
        background: var(--_bar, var(--_tessera-bar));
      }
      :host([highlighting]) {
        --_bar: var(--_tessera-bar-highlight);
      }
      [part='count'] {
        text-align: right;
        font-size: 12px;
        font-variant-numeric: tabular-nums;
        white-space: nowrap;
        color: var(--_tessera-ink-3);
      }
      [part='count'] .sub {
        font-weight: 500;
        color: var(--_tessera-ink);
      }
      :host([highlighting]) [part='count'] .sub {
        color: var(--_tessera-highlight);
      }
      /* The verbs share the counts' cell and take their place while the row is hovered or holds
         focus. Hidden, they stay in the tab order, so Tab reaches them and shows them. */
      [part='count'],
      [part='verbs'] {
        grid-column: 3;
        grid-row: 1;
      }
      [part='verbs'] {
        display: flex;
        justify-self: end;
        gap: 4px;
        opacity: 0;
        pointer-events: none;
      }
      [part~='row']:hover [part='verbs'],
      [part~='row']:focus-within [part='verbs'],
      [part~='row'][data-shown] [part='verbs'] {
        opacity: 1;
        pointer-events: auto;
      }
      [part~='row']:hover [part='count'],
      [part~='row']:focus-within [part='count'],
      [part~='row'][data-shown] [part='count'] {
        visibility: hidden;
      }
      [part='verbs'] button {
        width: 22px;
        height: 22px;
        display: grid;
        place-items: center;
        border: 1px solid var(--_tessera-line);
        border-radius: 5px;
        background: var(--_tessera-surface);
        color: color-mix(in srgb, var(--_tessera-ink) 82%, var(--_tessera-surface));
      }
      [part='verbs'] button[aria-pressed='true'] {
        border-color: var(--_tessera-accent);
        background: var(--_tessera-accent);
        color: var(--_tessera-accent-ink);
      }
      [part='verbs'] [part='highlight'][aria-pressed='true'] {
        border-color: var(--_tessera-highlight);
        background: var(--_tessera-highlight);
      }
      [part='more'] {
        align-self: flex-start;
        margin: 4px 0 0 20px;
      }
      .none {
        padding: 4px 0 0 20px;
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
      /* The histogram. */
      .hist {
        position: relative;
        margin-top: 8px;
      }
      [part='plot'] {
        position: relative;
        height: ${PLOT_HEIGHT}px;
        display: flex;
        align-items: flex-end;
        gap: 2px;
        cursor: crosshair;
        touch-action: none;
        user-select: none;
        border-radius: 2px;
      }
      [part~='bin'] {
        position: relative;
        flex: 1 1 0;
        height: ${PLOT_HEIGHT}px;
      }
      [part~='bin'] span {
        position: absolute;
        bottom: 0;
        border-radius: 1px 1px 0 0;
      }
      [part~='bin'] [part='bar-match'] {
        left: 0;
        right: 0;
      }
      [part~='bin'] [part='bar-subject'] {
        left: 2px;
        right: 2px;
      }
      [part~='band'] {
        position: absolute;
        top: -4px;
        bottom: -2px;
        border-radius: 4px;
        border: 1.5px solid var(--_tessera-ink);
        background: color-mix(in srgb, var(--_tessera-ink) 6%, transparent);
        pointer-events: none;
      }
      [part~='band'][data-verb='brush'] {
        border-style: dashed;
      }
      [part~='band'][data-verb='highlight'] {
        border-color: var(--_tessera-highlight);
        background: color-mix(in srgb, var(--_tessera-highlight) 8%, transparent);
      }
      [part='ramp'] {
        height: 6px;
        margin-top: 4px;
        border-radius: 2px;
      }
      [part='axis'] {
        display: flex;
        justify-content: space-between;
        margin-top: 4px;
        font-size: 12px;
        color: var(--_tessera-ink-2);
        font-variant-numeric: tabular-nums;
      }
      [part='brush'] {
        position: absolute;
        top: ${PLOT_HEIGHT + 8}px;
        z-index: 2;
        display: flex;
        flex-direction: column;
        gap: 8px;
        padding: 10px;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: 0 6px 20px rgba(0, 0, 0, 0.12);
        font-size: 12px;
        white-space: nowrap;
        color: var(--_tessera-ink-2);
      }
      [part='brush'] b {
        color: var(--_tessera-ink);
        font-weight: 600;
      }
      [part='brush'] .acts {
        display: flex;
        gap: 6px;
      }
      [part='brush'] .acts button {
        display: flex;
        align-items: center;
        gap: 6px;
        padding: 5px 10px;
        border-radius: 6px;
        font-size: 12px;
        font-weight: 500;
      }
      [part='brush-filter'] {
        background: var(--_tessera-accent);
        color: var(--_tessera-accent-ink);
      }
      [part='brush-highlight'] {
        border: 1px solid color-mix(in srgb, var(--_tessera-highlight) 18%, var(--_tessera-highlight-soft)) !important;
        background: var(--_tessera-highlight-soft);
        color: var(--_tessera-highlight);
      }
      .text {
        margin-top: 6px;
      }
      /* Read out, not drawn. */
      .hidden {
        position: absolute;
        width: 1px;
        height: 1px;
        overflow: hidden;
        clip-path: inset(50%);
        white-space: nowrap;
      }
      /* The colour picker, in the top layer beside the card. */
      .pop {
        position: fixed;
        inset: auto;
        margin: 0;
        padding: 0;
        width: 264px;
        box-sizing: border-box;
        background: var(--_tessera-surface);
        color: var(--_tessera-ink);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: 0 6px 24px rgba(0, 0, 0, 0.1);
        font-size: 13px;
        max-height: calc(100vh - 16px);
        overflow-y: auto;
      }
      .pop button:focus-visible {
        outline-offset: -2px;
      }
      .pop .top {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 8px;
        padding: 12px 14px 8px;
      }
      .pop .top .t {
        font-weight: 600;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      .pop .choices {
        display: grid;
        gap: 6px;
        padding: 0 14px 10px;
      }
      [part='choice'] {
        aspect-ratio: 1;
        width: 100%;
        border-radius: 5px;
      }
      [part='choice'][aria-pressed='true'] {
        box-shadow:
          0 0 0 2px var(--_tessera-surface),
          0 0 0 3.5px var(--_tessera-ink);
      }
      .pop .hd {
        margin: 0;
        padding: 10px 14px 4px;
        border-top: 1px solid var(--_tessera-line-2);
      }
      .pop .custom {
        display: flex;
        flex-direction: column;
        gap: 8px;
        padding: 4px 14px 14px;
      }
      [part='sv'],
      [part='hue'] {
        position: relative;
        touch-action: none;
        cursor: crosshair;
      }
      [part='sv'] {
        height: 120px;
        border-radius: var(--_tessera-radius-control);
      }
      [part='hue'] {
        height: 10px;
        border-radius: 5px;
        background: linear-gradient(90deg, #ff0000, #ffff00, #00ff00, #00ffff, #0000ff, #ff00ff, #ff0000);
      }
      .knob {
        position: absolute;
        width: 12px;
        height: 12px;
        margin: -6px 0 0 -6px;
        border: 2px solid #ffffff;
        border-radius: 50%;
        box-shadow: 0 0 0 1px rgba(0, 0, 0, 0.3);
        pointer-events: none;
      }
      [part='hue'] .knob {
        top: 50%;
      }
      .pop label {
        display: grid;
        grid-template-columns: 28px minmax(0, 1fr);
        align-items: center;
        gap: 8px;
        font-size: 12px;
        color: var(--_tessera-ink-2);
      }
      [part='hex'] {
        font-size: 12px;
        font-variant-numeric: tabular-nums;
      }
    `
  ];

  /** The field: a column `meta.filterOperands` lists, or `cluster:<layer>` for a layer's clusters. */
  @property() accessor field = '';
  /** Draws the heading alone, with a small chart of the subject's counts. */
  @property({type: Boolean, reflect: true}) accessor folded = false;
  /** Draws the small chart for the compact layout. */
  @property({type: Boolean, reflect: true}) accessor compact = false;
  /** The level a levelled layer's card counts at; `null` is its deepest. */
  @property({type: Number, attribute: 'cluster-level'}) accessor level: number | null = null;

  /** Whether "N more" was pressed. @internal */
  @state() accessor expanded = false;
  /** The row whose verbs a press showed, on a screen without hover. @internal */
  @state() accessor shownRow: string | null = null;
  /** A range being chosen on the histogram. @internal */
  @state() accessor brush: Brush | null = null;
  /** The value the colour picker is open for. @internal */
  @state() accessor picking: Picking | null = null;
  /** The colour picker's custom colour. @internal */
  @state() accessor hsv: Hsv = [0, 0, 0];

  private readonly subjectCounts = new HeldAggregate('field-subject');
  private readonly matchCounts = new HeldAggregate('field-match');
  private readonly figures = new HeldAggregate('field-summary');
  /** The whole match's counts of the values in view its commonest do not list. */
  private readonly outsideCounts = new HeldAggregate('field-outside');
  private unwatchChoices: (() => void) | null = null;
  /** Every cluster the card has met on a browse page, for names and parents. */
  private readonly met = new Map<bigint, BrowseRow>();
  /** Each cluster's served parents, once asked for. */
  private readonly parentsOf = new Map<bigint, bigint[]>();
  /** The browse pages asked for, by form: a cluster's parents, a parent's children, a level's roots. */
  private readonly asked = new Set<string>();
  /** Moved by {@link resetServerData}, so an answer asked for before is dropped. */
  private epoch = 0;

  /** What the card last drew from, so a publish that changes none of it draws nothing. */
  private drawnFrom: readonly unknown[] = [];

  /**
   * Draw again only when what the card reads changed: its counts, the clauses, the colouring, the
   * view, or the colours of a layer's clusters, and on a cluster card the clusters the map draws,
   * which name its rows. The store publishes as each frame arrives, which changes none of these.
   */
  protected override onStoreChange(): void {
    const s = this.resolvedStore;
    const drawn = s && this.layerName !== null ? [s.get('artifacts').served, s.get('artifacts').colourServed] : [];
    const now = s ? [s.get('aggregates'), s.get('filters'), s.get('legend'), s.get('meta'), s.get('view').id, s.get('artifacts').colours, ...drawn] : [];
    if (now.length === this.drawnFrom.length && now.every((v, i) => v === this.drawnFrom[i])) return;
    this.drawnFrom = now;
    super.onStoreChange();
  }

  protected override onStoreAdopted(store: Store | null): void {
    this.drawnFrom = [];
    this.unwatchChoices?.();
    this.unwatchChoices = store ? watchChoices(store, () => this.requestUpdate()) : null;
  }

  protected override resetServerData(): void {
    this.epoch += 1;
    this.met.clear();
    this.parentsOf.clear();
    this.asked.clear();
    this.brush = null;
    this.closePicker(false);
  }

  override disconnectedCallback(): void {
    this.subjectCounts.set(null, null);
    this.matchCounts.set(null, null);
    this.outsideCounts.set(null, null);
    this.figures.set(null, null);
    this.closePicker(false);
    document.removeEventListener('pointerdown', this.onPressOutsideBrush, true);
    if (this.livePick) cancelAnimationFrame(this.livePick.frame);
    this.livePick = null;
    super.disconnectedCallback();
  }

  /** The layer a cluster card shows; `null` on a column's card. */
  private get layerName(): string | null {
    return this.field.startsWith(CLUSTER_PREFIX) ? this.field.slice(CLUSTER_PREFIX.length) : null;
  }

  private declaredLayer(meta: Meta | null): Layer | null {
    const name = this.layerName;
    return name === null ? null : (meta?.layers.find((l) => l.name === name) ?? null);
  }

  /** What the card draws, from the field's family in `meta`; `null` where `meta` offers no such field. */
  private kindOf(meta: Meta | null): Kind | null {
    if (!meta) return null;
    if (this.layerName !== null) return this.declaredLayer(meta) ? 'cluster' : null;
    const o = meta.filterOperands.find((x) => x.column === this.field);
    if (!o) return null;
    return o.family === 'category' ? 'category' : o.family === 'numeric' ? 'histogram' : 'search';
  }

  private isTimestamp(meta: Meta | null): boolean {
    return meta?.declaredScalars.find((c) => c.name === this.field)?.arrowType === 'timestamp_us';
  }

  /** Whether the map can be coloured by this field. */
  private colourable(meta: Meta, kind: Kind): boolean {
    const name = this.layerName;
    if (name !== null) return colourLayers(meta.layers).some((l) => l.name === name);
    return (kind === 'category' || kind === 'histogram') && meta.declaredScalars.some((c) => c.name === this.field && c.render);
  }

  /** The level a levelled layer's card counts and browses at: the one chosen, else the deepest. */
  private levelOf(layer: Layer): number | undefined {
    const levelled = (layer.hierarchy.kind === 'stacked' || layer.hierarchy.kind === 'tiered') && layer.levels.length > 0;
    if (!levelled) return undefined;
    const chosen = this.level !== null && layer.levels.some((l) => l.level === this.level) ? this.level : null;
    return chosen ?? layer.levels.at(-1)!.level;
  }

  /** The keys of the field's clause in `verb`: a category's values, or a layer's clusters by id. */
  private clauseKeys(store: Store, verb: ClauseVerb): Set<string> {
    const name = this.layerName;
    if (name !== null) return new Set(store.get('filters').members.filter((m) => m.layer === name && m.verb === verb && !m.outside).map((m) => idString(m.artifact)));
    const held = store.get('filters').draft[verb][this.field];
    return new Set(held?.family === 'category' ? held.keys : []);
  }

  /** The groupings a category or cluster card counts; the values its clauses name are always counted. */
  private rowGroupings(store: Store, meta: Meta, kind: Kind, top: number): AggregateSpec['groupings'] | null {
    const named = [...new Set([...this.clauseKeys(store, 'filter'), ...this.clauseKeys(store, 'highlight')])];
    if (kind === 'category') {
      const values = named.slice(0, meta.selection.maxAggregateNamed).sort();
      return [{by: {field: this.field, top}}, ...(values.length > 0 ? [{by: {field: this.field, values}}] : [])];
    }
    const layer = this.declaredLayer(meta);
    if (!layer) return null;
    return [this.ranked(layer, top), ...this.namedGroupings(layer, meta, named.map((id) => BigInt(id)))];
  }

  /**
   * The grouping ranking a layer's `top` clusters: at the cut the map draws on a `nested` or `dag`
   * layer, else at the card's level.
   */
  private ranked(layer: Layer, top: number): AggregateSpec['groupings'][number] {
    if (layer.hierarchy.kind === 'nested' || layer.hierarchy.kind === 'dag') return {by: {layer: layer.name, top, cut: 'drawn'}};
    const level = this.levelOf(layer);
    return {by: {layer: layer.name, ...(level === undefined || !countedByLevel(layer) ? {} : {level}), top}};
  }

  /** The groupings counting `ids` of `layer` by name, leaving room for one more beside them. */
  private namedGroupings(layer: Layer, meta: Meta, ids: bigint[]): AggregateSpec['groupings'] {
    const level = this.levelOf(layer) ?? 0;
    const rows = [...new Set(ids)].map((id) => ({tesseraId: id, rung: this.met.get(id)?.rung ?? level}));
    return rows.length === 0 ? [] : artifactGroupings(layer, rows, meta.selection).slice(0, meta.selection.maxAggregateGroupings - 1);
  }

  /**
   * The values in view that the whole match's commonest values do not list, which a registration of
   * their own asks for by name, so each row has both its counts. It reads the two answers and
   * neither reads it, so asking does not change what is asked. Nothing is named while either
   * answer is held for another store, as just after the card adopts a new one.
   */
  private outsideSpec(store: Store, meta: Meta): AggregateSpec | null {
    const matchTop = this.matchCounts.entryFor(store)?.result?.tables[0];
    const inView = this.subjectCounts.entryFor(store)?.result?.tables[0];
    if (!matchTop || !inView) return null;
    const listed = new Set(listedGroups(matchTop).map((g) => g.key));
    const named = new Set([...this.clauseKeys(store, 'filter'), ...this.clauseKeys(store, 'highlight')]);
    const values = listedGroups(inView)
      .map((g) => g.key)
      .filter((k) => !listed.has(k) && !named.has(k))
      .slice(0, meta.selection.maxAggregateNamed)
      .sort();
    return values.length === 0 ? null : {groupings: [{by: {field: this.field, values}}], without: this.field};
  }

  /** The specs the card keeps registered: the subject's counts, the whole match's and a number's figures. */
  private specs(store: Store, meta: Meta, kind: Kind): {subject: AggregateSpec | null; match: AggregateSpec | null; figures: AggregateSpec | null} {
    const leaveOut = this.layerName !== null ? {withoutMembersOf: this.layerName} : {without: this.field};
    if (kind === 'search') return {subject: null, match: null, figures: null};
    if (kind === 'histogram') {
      const bins = [{by: {field: this.field, bins: BINS, sample: SAMPLE}}];
      return {
        subject: {groupings: bins, subject: 'view', highlighted: true, ...leaveOut},
        match: this.folded ? null : {groupings: bins, ...leaveOut},
        figures: {groupings: [{by: {field: this.field, summary: true}}], subject: 'visible'}
      };
    }
    const subject = this.rowGroupings(store, meta, kind, this.expanded ? MORE_ROWS : ROWS);
    // A layer's whole match counts the clusters the subject lists and its clauses name, and ranks
    // one more, whose table says how many there are; the search box names that number.
    const layer = this.declaredLayer(meta);
    const listed = [...(countsByKey(this.subjectCounts.entryFor(store))?.keys() ?? []), ...this.clauseKeys(store, 'filter'), ...this.clauseKeys(store, 'highlight')];
    const match =
      kind === 'category'
        ? this.rowGroupings(store, meta, kind, Math.min(MATCH_TOP, meta.selection.maxAggregateTop))
        : layer
          ? [...this.namedGroupings(layer, meta, listed.map((id) => BigInt(id))), this.ranked(layer, 1)]
          : null;
    return {
      subject: subject ? {groupings: subject, subject: 'view', highlighted: true, ...leaveOut} : null,
      match: match && !this.folded ? {groupings: match, ...leaveOut} : null,
      figures: null
    };
  }

  protected override updated(changed: PropertyValues<this>): void {
    super.updated(changed);
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    const kind = this.kindOf(meta);
    this.toggleAttribute('highlighting', s ? this.highlighting(s) : false);
    const live = this.isConnected && s !== null && meta !== null && kind !== null;
    const specs = live ? this.specs(s, meta, kind) : {subject: null, match: null, figures: null};
    this.subjectCounts.set(s, specs.subject);
    this.matchCounts.set(s, specs.match);
    this.outsideCounts.set(s, live && kind === 'category' && !this.folded ? this.outsideSpec(s, meta) : null);
    this.figures.set(s, specs.figures);
    this.placePicker(changed);
    if (changed.has('brush')) {
      if (this.brush?.open) document.addEventListener('pointerdown', this.onPressOutsideBrush, true);
      else document.removeEventListener('pointerdown', this.onPressOutsideBrush, true);
    }
  }

  /** A press outside an open range box closes it, as Escape does. */
  private onPressOutsideBrush = (e: PointerEvent): void => {
    const box = this.renderRoot.querySelector('[part="brush"]');
    if (box && e.composedPath().includes(box)) return;
    this.brush = null;
  };

  /** Whether a highlight is set, in which case the subject is the highlighted items. */
  private highlighting(store: Store): boolean {
    const {draft, members} = store.get('filters');
    return Object.values(draft.highlight).some((d) => isPopulated(d)) || members.some((m) => m.verb === 'highlight');
  }

  /**
   * Ask the layer's browse pages for what names the clusters in `ids` the map does not: on a
   * `nested` or `dag` layer each cluster's served parents, which name its path, and then the
   * children of its first parent, or the roots where it has none, which name it; on any other
   * layer the largest clusters at the card's level. Each page is asked for once.
   */
  private askNames(s: Store, meta: Meta, layer: Layer, ids: bigint[], named: (id: bigint) => boolean): void {
    const treed = layer.hierarchy.kind === 'nested' || layer.hierarchy.kind === 'dag';
    const level = this.levelOf(layer);
    const limit = Math.max(1, meta.selection.maxBrowseRows);
    const epoch = this.epoch;
    const page = (form: string, req: {parent?: bigint; level?: number; limit: number}, took: (rows: BrowseRow[], parents: BrowseRow[]) => void) => {
      if (this.asked.has(form)) return;
      this.asked.add(form);
      void s
        .browse({layer: layer.name, filters: null, ...req})
        .then((p) => {
          if (epoch !== this.epoch) return;
          for (const r of [...p.parents, ...p.artifacts]) this.met.set(r.tesseraId, r);
          took(p.artifacts, p.parents);
          this.requestUpdate();
        })
        .catch(() => undefined);
    };
    for (const id of ids) {
      if (!treed) {
        if (!named(id)) page(`level:${level ?? ''}`, {limit, ...(level === undefined ? {} : {level})}, () => {});
        continue;
      }
      const parents = this.parentsOf.get(id);
      if (parents === undefined) {
        page(`parents:${id}`, {parent: id, limit: 1}, (_, ps) => this.parentsOf.set(id, ps.map((r) => r.tesseraId)));
        continue;
      }
      if (named(id)) continue;
      if (parents.length > 0) page(`children:${parents[0]}`, {parent: parents[0]!, limit}, () => {});
      else page('roots', {limit}, () => {});
    }
  }

  /** A cluster's parents' names, nearest last, as far up as the card has met them. */
  private pathOf(id: bigint): string {
    const names: string[] = [];
    let parents = this.parentsOf.get(id) ?? this.met.get(id)?.parentIds ?? [];
    for (let i = 0; i < 2 && parents.length > 0; i++) {
      const at = this.met.get(parents[0]!);
      if (!at) break;
      names.unshift(artifactName(at) ?? UNNAMED);
      parents = this.parentsOf.get(at.tesseraId) ?? at.parentIds;
    }
    return names.join(' › ');
  }

  // ---- clauses ---------------------------------------------------------------------------------

  /** Put a category value in or out of the field's clause in `verb`. */
  private toggleValue(key: string, verb: ClauseVerb): void {
    const s = this.resolvedStore;
    if (!s) return;
    const draft = s.get('filters').draft;
    const held = draft[verb][this.field];
    const was = held?.family === 'category' ? held.keys : [];
    const keys = was.includes(key) ? was.filter((k) => k !== key) : [...was, key];
    this.setClause(verb, {family: 'category', keys});
  }

  /** Replace the field's clause in `verb`, and report what that position composes. */
  private setClause(verb: ClauseVerb, clause: ColumnDraft): void {
    const s = this.resolvedStore;
    if (!s) return;
    const draft = s.get('filters').draft;
    const next: FilterDraft = {...draft, [verb]: {...draft[verb], [this.field]: clause}};
    s.setFilters(next);
    emit(this, 'tessera-filterchange', {column: this.field, verb, expr: composeFilters(next, verb)});
  }

  /** Put a cluster's `member_of` clause in `verb` on or off. */
  private toggleCluster(id: string, name: string | null, verb: ClauseVerb): void {
    const s = this.resolvedStore;
    const layer = this.layerName;
    if (!s || layer === null) return;
    const artifact = BigInt(id);
    const members = s.get('filters').members;
    const on = members.some((m) => m.layer === layer && m.artifact === artifact && m.verb === verb && !m.outside);
    s.setMembers(on ? withoutMember(members, layer, artifact, verb) : withMember(members, {layer, artifact, outside: false, verb, ...(name === null ? {} : {label: name})}));
    emit(this, 'tessera-clausechange', {id, layer, outside: false, verb, on: !on});
  }

  /** Colour the map by this field, or by nothing where it already is. */
  private paint(): void {
    const s = this.resolvedStore;
    if (!s) return;
    const colourBy = s.get('legend').colourBy === this.field ? null : this.field;
    s.setColourBy(colourBy);
    emit(this, 'tessera-colourchange', {colourBy});
  }

  private chooseLevel(value: string): void {
    const level = value === '' ? null : Number(value);
    this.level = level;
    emit(this, 'tessera-levelchange', {level});
  }

  private fold(): void {
    this.folded = !this.folded;
    this.brush = null;
    emit(this, 'tessera-fold', {field: this.field, folded: this.folded});
  }

  // ---- render ----------------------------------------------------------------------------------

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    const kind = this.kindOf(meta);
    if (!s || !meta || !kind) return nothing;
    const layer = this.declaredLayer(meta);
    const title = layer ? layer.title || layer.name : columnCaption(this.field);
    const colouring = s.get('legend').colourBy === this.field;
    const paint = this.colourable(meta, kind)
      ? html`<button part="paint" type="button" aria-pressed=${colouring ? 'true' : 'false'} aria-label=${`Colour by ${title}`} title=${colouring ? 'Stop colouring by this' : 'Colour by this'} @click=${() => this.paint()}>${icon('drop', 13, 2)}</button>`
      : nothing;
    const fold = html`<button part="fold" type="button" aria-expanded=${this.folded ? 'false' : 'true'} aria-label=${this.folded ? `Open ${title}` : `Fold ${title}`} @click=${() => this.fold()}>${icon(this.folded ? 'chev' : 'chevup', 12, 2)}</button>`;
    const head = (sub: TemplateResult | string | typeof nothing) =>
      html`<div part="head"><span part="title">${title}</span>${this.folded && this.compact && colouring ? html`<span class="painted" role="img" aria-label="Colouring the map">${icon('drop', 11, 2)}</span>` : nothing}${sub}${this.folded ? this.spark(s, kind, colouring) : nothing}${paint}${fold}</div>`;
    if (kind === 'histogram') return this.histogram(s, meta, head, colouring);
    if (kind === 'search') {
      const body = this.folded ? nothing : html`<tessera-filter class="text" exportparts=${exportparts('filter')} column=${this.field} placeholder=${`Search ${title.toLowerCase()}`} .store=${s}></tessera-filter>`;
      return html`${head(html`<span part="sub"></span>`)}${body}`;
    }
    return this.valueCard(s, kind, layer, head, colouring);
  }

  /** A folded card's small chart: the subject's counts, largest first for values, in order for bins. */
  private spark(s: Store, kind: Kind, colouring: boolean): TemplateResult | typeof nothing {
    const entry = this.subjectCounts.entry();
    const table = entry?.result?.tables[0];
    if (!table) return html`<span part="spark" aria-hidden="true"></span>`;
    let bars: {n: number; colour: string | null}[];
    if (kind === 'histogram') {
      const bins = binsOf(table, this.isTimestamp(s.get('meta')));
      const per = Math.max(1, Math.ceil(bins.length / SPARK_BARS));
      bars = [];
      for (let i = 0; i < bins.length; i += per) bars.push({n: bins.slice(i, i + per).reduce((t, b) => t + b.count, 0), colour: null});
    } else {
      const counts = countsByKey(entry);
      const rows = this.rows(s, kind, entry, undefined, colouring).slice(0, SPARK_BARS);
      bars = rows.map((r) => ({n: counts?.get(r.key) ?? 0, colour: r.swatch}));
    }
    const top = Math.max(1, ...bars.map((b) => b.n));
    return html`<span part="spark" aria-hidden="true">${bars.map((b) => html`<span style=${`height:${Math.max(1, Math.round((b.n / top) * 14))}px${b.colour ? `;background:${b.colour}` : ''}`}></span>`)}</span>`;
  }

  /**
   * The rows of a category or cluster card: the values its clauses name, then the rest by their
   * count in the subject, then in the whole match.
   */
  private rows(s: Store, kind: Kind, subject: AggregateEntry | undefined, match: AggregateEntry | undefined, colouring: boolean): Row[] {
    const sub = countsByKey(subject);
    const all = countsByKey(match);
    if (all && kind === 'category') for (const [k, n] of countsByKey(this.outsideCounts.entryFor(s)) ?? []) if (!all.has(k)) all.set(k, n);
    const named = [...this.clauseKeys(s, 'filter'), ...this.clauseKeys(s, 'highlight')];
    const colours = colouringOf(s);
    if (kind === 'category') {
      const titles = new Map<string, string | null>();
      for (const t of [...(subject?.result?.tables ?? []), ...(match?.result?.tables ?? [])]) for (const g of listedGroups(t)) if (!titles.has(g.key) || g.title) titles.set(g.key, g.title);
      const order = [...new Set([...named, ...(sub ? [...sub.keys()] : []), ...(all ? [...all.keys()] : [])])];
      const keys = order.filter((k) => named.includes(k) || (sub?.get(k) ?? 0) > 0 || (!sub && (all?.get(k) ?? 0) > 0));
      keys.sort((a, b) => Number(named.includes(b)) - Number(named.includes(a)) || (sub?.get(b) ?? 0) - (sub?.get(a) ?? 0) || (all?.get(b) ?? 0) - (all?.get(a) ?? 0));
      const legend = s.get('legend');
      return keys.map((key) => ({
        key,
        name: titles.get(key) ?? keyTitle(s, this.field, key),
        path: '',
        sub: sub?.get(key) ?? null,
        all: all?.get(key) ?? null,
        swatch: colouring ? this.valueColour(key, legend.categories[this.field] ?? [], legend.ranks[this.field] ?? {}, colours) : null
      }));
    }
    const layer = this.declaredLayer(s.get('meta'));
    const ids = [...new Set([...named, ...(sub ? [...sub.keys()] : [])])];
    const keys = ids.filter((k) => named.includes(k) || (sub?.get(k) ?? 0) > 0);
    keys.sort((a, b) => Number(named.includes(b)) - Number(named.includes(a)) || (sub?.get(b) ?? 0) - (sub?.get(a) ?? 0) || (all?.get(b) ?? 0) - (all?.get(a) ?? 0));
    const artifacts = s.get('artifacts');
    return keys.map((key) => {
      const id = BigInt(key);
      let swatch: string | null = null;
      if (colouring && layer) swatch = rgb(artifacts.colours.get(artifacts.table.ordinalOf(layer.name, id)) ?? NEUTRAL);
      return {key, name: this.nameOf(s, id) ?? UNNAMED, path: this.pathOf(id), sub: sub?.get(key) ?? null, all: all?.get(key) ?? null, swatch};
    });
  }

  /** A cluster's name: as the map names it, else as a browse page did, else as its clause does. */
  private nameOf(s: Store, id: bigint): string | null {
    const artifacts = s.get('artifacts');
    const drawn = [...artifacts.served, ...artifacts.colourServed].find((a) => a.layer === this.layerName && a.tesseraId === id);
    const row = this.met.get(id);
    return (
      (drawn ? artifactName(drawn, artifacts.attached) : null) ??
      (row ? artifactName(row) : null) ??
      s.get('filters').members.find((m) => m.artifact === id && m.layer === this.layerName)?.label ??
      null
    );
  }

  /** A category value's colour on the map: the one chosen for it, else its palette colour by rank. */
  private valueColour(key: string, values: readonly {key: string; code: number}[], ranks: Record<number, number>, colouring: Colouring): string {
    const chosen = colouring.values[this.field]?.[key];
    if (chosen) return chosen;
    const code = values.find((v) => v.key === key)?.code;
    return code === undefined ? rgb(UNMAPPED) : rgb(colourOfRank(ranks[code], colouring.palette));
  }

  private valueCard(s: Store, kind: Kind, layer: Layer | null, head: (sub: TemplateResult | typeof nothing) => TemplateResult, colouring: boolean): TemplateResult {
    const levelled = layer ? this.levelOf(layer) : undefined;
    const sub =
      layer && levelled !== undefined && layer.levels.length > 1
        ? html`<span part="sub" class="choice"><select part="level-select" aria-label="Level" @change=${(e: Event) => this.chooseLevel((e.target as HTMLSelectElement).value)}>
              <option value="" ?selected=${this.level === null}>Deepest level</option>
              ${layer.levels.map((l) => html`<option value=${l.level} ?selected=${this.level === l.level}>${l.title || `Level ${l.level}`}</option>`)}
            </select>${icon('chev', 12, 1.4)}</span>`
        : html`<span part="sub"></span>`;
    if (this.folded) return head(sub);
    const subject = this.subjectCounts.entry();
    const match = this.matchCounts.entry();
    const all = this.rows(s, kind, subject, match, colouring);
    const filtered = this.clauseKeys(s, 'filter');
    const lit = this.clauseKeys(s, 'highlight');
    const shown = all.slice(0, Math.max(this.expanded ? MORE_ROWS : ROWS, all.filter((r) => filtered.has(r.key) || lit.has(r.key)).length));
    const meta = s.get('meta');
    if (layer && meta) this.askNames(s, meta, layer, shown.map((r) => BigInt(r.key)), (id) => this.nameOf(s, id) !== null);
    // The shares are of each set's total, or of what the field's own filter keeps where it has one.
    const keptTotal = (counts: Map<string, number> | null, table: AggregateTable | undefined) =>
      filtered.size > 0 && counts ? [...filtered].reduce((t, k) => t + (counts.get(k) ?? 0), 0) : (table?.total ?? 0);
    const subTotal = keptTotal(countsByKey(subject), subject?.result?.tables[0]);
    const allTotal = keptTotal(countsByKey(match), match?.result?.tables[0]);
    const same = subTotal === allTotal;
    const pct = (n: number | null, total: number) => (n === null || total <= 0 ? 0 : Math.min(100, (100 * n) / total));
    // A layer's clusters are often a few per cent each; a bar that is not empty stays visible.
    const width = (share: number) => (share > 0 && kind === 'cluster' ? `max(${MIN_BAR_PX}px, ${share.toFixed(1)}%)` : `${share.toFixed(1)}%`);
    const groups = kind === 'category' ? (subject?.result?.tables[0]?.groups ?? null) : all.length;
    const more = groups === null ? 0 : groups - shown.length;
    const editable = kind === 'category' && colouring;
    const row = (r: Row) => {
      const out = filtered.size > 0 && !filtered.has(r.key);
      const inFilter = filtered.has(r.key);
      const onLit = lit.has(r.key);
      const stateOf = out ? 'out' : inFilter ? 'in' : onLit ? 'lit' : '';
      const mark = r.swatch
        ? editable
          ? html`<button part="swatch" type="button" style=${`--c:${r.swatch}`} aria-haspopup="dialog" aria-label=${`Colour of ${r.name}`} @click=${(e: Event) => this.openPicker(e, {key: r.key, title: r.name}, r.swatch!)}></button>`
          : html`<span part="swatch" style=${`--c:${r.swatch}`}></span>`
        : inFilter
          ? icon('check', 12, 2.6)
          : nothing;
      const verb = (v: ClauseVerb) => () => (kind === 'category' ? this.toggleValue(r.key, v) : this.toggleCluster(r.key, r.name === UNNAMED ? null : r.name, v));
      const counts = out
        ? html`<span part="count">${r.all === null ? '' : countText(r.all)}</span>`
        : html`<span part="count"><span class="sub">${r.sub === null ? '' : countText(r.sub)}</span>${same || r.all === null ? '' : ` / ${countText(r.all)}`}</span>`;
      return html`<div part="row" data-key=${r.key} data-state=${stateOf} ?data-shown=${this.shownRow === r.key}
        @click=${(e: Event) => {
          if ((e.target as HTMLElement).closest('button')) return;
          this.shownRow = this.shownRow === r.key ? null : r.key;
        }}>
        <span class="mark">${mark}</span>
        <span class="label">
          <span class="names"><span part="name" title=${r.name} ?data-unnamed=${r.name === UNNAMED}>${r.name}</span>${r.path ? html`<span part="path" title=${r.path}>${r.path}</span>` : nothing}</span>
          <span class="bars" aria-hidden="true"><span part="bar-match" style=${`width:${width(out ? 0 : pct(r.all, allTotal))}`}></span><span part="bar-subject" style=${`width:${width(out ? 0 : pct(r.sub, subTotal))}${r.swatch ? `;background:${r.swatch}` : ''}`}></span></span>
        </span>
        ${counts}
        <span part="verbs">
          <button part="highlight" type="button" aria-pressed=${onLit ? 'true' : 'false'} aria-label=${onLit ? `Stop highlighting ${r.name}` : `Highlight ${r.name}`} title=${onLit ? 'Stop highlighting' : 'Highlight'} @click=${verb('highlight')}>${icon('highlight', 13, 1.5)}</button>
          <button part="filter" type="button" aria-pressed=${inFilter ? 'true' : 'false'} aria-label=${inFilter ? `Stop filtering to ${r.name}` : `Filter to ${r.name}`} title=${inFilter ? 'Stop filtering' : 'Filter'} @click=${verb('filter')}>${icon('filter', 13, 1.6)}</button>
        </span>
      </div>`;
    };
    const values = (kind === 'category' ? match?.result?.tables[0] : match?.result?.tables.at(-1))?.groups ?? null;
    const search =
      kind === 'category'
        ? html`<tessera-filter class="search" exportparts=${exportparts('filter')} column=${this.field} placeholder=${values === null ? 'Search values' : `Search ${values.toLocaleString('en-GB')} values`} .store=${s}></tessera-filter>`
        : html`<tessera-cluster-filter class="search" exportparts=${exportparts('cluster-filter')} layer=${layer!.name} placeholder=${values === null ? 'Search clusters' : `Search ${values.toLocaleString('en-GB')} clusters`} .store=${s}></tessera-cluster-filter>`;
    const moreButton =
      more > 0 || this.expanded
        ? html`<button part="more" class="more-link" type="button" @click=${() => (this.expanded = !this.expanded)}>${this.expanded ? 'Show fewer' : `${more.toLocaleString('en-GB')} more${this.highlighting(s) ? '' : ' in view'}`}</button>`
        : nothing;
    // An answer with nothing in it says so, where a list would otherwise stand empty.
    const empty = shown.length === 0 && subject?.result ? html`<span class="none">${subTotal === 0 ? 'None in view' : 'None counted'}</span>` : nothing;
    return html`${head(sub)}<div class="body">${search}<div part="rows" role="list" aria-label=${`Commonest ${layer ? 'clusters' : 'values'}`}>${shown.map(
        (r) => html`<div role="listitem">${row(r)}</div>`
      )}</div>${empty}${moreButton}</div>${this.picking ? this.colourPicker(s, this.picking) : nothing}`;
  }

  // ---- histogram ---------------------------------------------------------------------------------

  private histogram(s: Store, meta: Meta, head: (sub: TemplateResult | string | typeof nothing) => TemplateResult, colouring: boolean): TemplateResult {
    const timestamp = this.isTimestamp(meta);
    const column = meta.declaredScalars.find((c) => c.name === this.field);
    const whole = column ? column.arrowType !== 'f32' && column.arrowType !== 'f64' : false;
    const value = (v: number) => (timestamp ? shortDateText(v) : formatNumber(v, whole));
    const summary = this.figures.entry()?.summaries[0] ?? null;
    const sub = html`<span part="sub" title=${this.summaryText(summary, timestamp, whole, false)}>${this.summaryText(summary, timestamp, whole, this.folded)}</span>`;
    if (this.folded) return head(sub);
    const subjectTable = this.subjectCounts.entry()?.result?.tables[0];
    const matchTable = this.matchCounts.entry()?.result?.tables[0];
    const subjectBins = binsOf(subjectTable, timestamp);
    const matchBins = binsOf(matchTable, timestamp);
    const bins = matchBins.length > 0 ? matchBins : subjectBins;
    const subTotal = subjectTable?.total ?? 0;
    const allTotal = matchTable?.total ?? 0;
    const subOf = (b: Bin) => subjectBins.find((x) => x.lower === b.lower)?.count ?? 0;
    const allOf = (b: Bin) => matchBins.find((x) => x.lower === b.lower)?.count ?? 0;
    const share = (n: number, total: number) => (total > 0 ? n / total : 0);
    const tallest = Math.max(1e-9, ...bins.map((b) => Math.max(share(allOf(b), allTotal), share(subOf(b), subTotal))));
    const height = (n: number, total: number) => `${((share(n, total) / tallest) * PLOT_HEIGHT).toFixed(1)}px`;
    const estimate = Boolean(subjectTable?.sample?.sampled || matchTable?.sample?.sampled);
    const approx = estimate ? '≈' : '';
    const lo = bins[0]?.lower;
    const hi = bins.at(-1)?.upper;
    const at = (v: number) => (lo === undefined || hi === undefined || hi <= lo ? 0 : Math.min(1, Math.max(0, (v - lo) / (hi - lo))));
    const pct = (t: number) => `${(t * 100).toFixed(2)}%`;
    const band = (verb: string, from: number, to: number) => html`<span part="band" data-verb=${verb} style=${`left:calc(${pct(from)} - 1px);width:calc(${pct(to - from)} + 2px)`}></span>`;
    const draft = s.get('filters').draft;
    const clauseBand = (verb: ClauseVerb) => {
      const held = draft[verb][this.field];
      if (held?.family !== 'numeric' || !isPopulated(held) || lo === undefined) return nothing;
      return band(verb, held.gte === null ? 0 : at(held.gte), held.lte === null ? 1 : at(held.lte));
    };
    const brush = this.brush;
    const range = brush && bins.length > 0 ? this.brushRange(bins, brush, timestamp, whole) : null;
    const brushBand = brush && bins.length > 0 ? band('brush', Math.min(brush.from, brush.to) / bins.length, (Math.max(brush.from, brush.to) + 1) / bins.length) : nothing;
    const ramp = colouring && bins.length > 0 ? this.ramp(s, bins) : nothing;
    const middle = bins[Math.floor(bins.length / 2)]?.lower;
    const axis =
      lo !== undefined && hi !== undefined
        ? html`<div part="axis"><span>${this.edgeText(lo, lo, hi, timestamp, whole)}</span><span>${middle === undefined ? '' : this.edgeText(middle, lo, hi, timestamp, whole)}</span><span>${this.edgeText(hi, lo, hi, timestamp, whole)}</span></div>`
        : html`<div part="axis"><span class="skel"></span></div>`;
    const box =
      range && brush?.open
        ? html`<div part="brush" role="dialog" aria-label="Chosen range" style=${this.brushPlace(brush, bins.length)} @keydown=${this.onBrushKey}>
            <div><b>${range.text}</b> · ${approx}${countText(range.count)} items</div>
            <div class="acts">
              <button part="brush-filter" type="button" @click=${() => this.applyBrush('filter', range)}>${icon('filter', 12, 2)}Filter</button>
              <button part="brush-highlight" type="button" @click=${() => this.applyBrush('highlight', range)}>${icon('highlight', 12, 1.8)}Highlight</button>
            </div>
          </div>`
        : nothing;
    const title = (b: Bin) => `${value(b.lower)} – ${value(b.upper)}: ${approx}${countText(subOf(b))} in view, ${approx}${countText(allOf(b))} matching`;
    // While a range is being chosen, a reader hears it and its count as it moves.
    const said = range && !brush?.open ? `${range.text}, ${approx}${countText(range.count)} items` : '';
    return html`${head(sub)}<div class="hist">
        <div part="plot" role="group" tabindex="0" aria-label=${`${columnCaption(this.field)}: drag or use Shift and the arrow keys to choose a range`}
          @pointerdown=${(e: PointerEvent) => this.brushStart(e, bins.length)} @pointermove=${(e: PointerEvent) => this.brushMove(e, bins.length)}
          @pointerup=${(e: PointerEvent) => this.brushEnd(e, bins.length)} @pointercancel=${() => (this.brush = null)} @keydown=${(e: KeyboardEvent) => this.brushKey(e, bins.length)}>
          ${bins.map(
            (b, i) => html`<div part="bin" data-index=${i} title=${title(b)}><span part="bar-match" style=${`height:${height(allOf(b), allTotal)}`}></span><span part="bar-subject" style=${`height:${height(subOf(b), subTotal)}`}></span></div>`
          )}
          ${clauseBand('filter')}${clauseBand('highlight')}${brushBand}
        </div>
        ${ramp}${axis}${box}
        <span class="hidden" role="status" aria-live="polite">${said}</span>
      </div>`;
  }

  /** Where the range's box stands: under the range, from its left end or, on the right half, its right, and never past the plot. */
  private brushPlace(brush: Brush, n: number): string {
    const from = Math.min(brush.from, brush.to) / n;
    const to = (Math.max(brush.from, brush.to) + 1) / n;
    const room = 'calc(100% - 210px)';
    return from + to < 1 ? `left:clamp(0px, ${(from * 100).toFixed(2)}%, ${room})` : `right:clamp(0px, ${((1 - to) * 100).toFixed(2)}%, ${room})`;
  }

  /** The figures in the heading: how many hold a value and their mean, or while folded their range. */
  private summaryText(summary: FieldSummary | null, timestamp: boolean, whole: boolean, folded: boolean): string {
    if (!summary || summary.min === null || summary.max === null) return '';
    const unit = timestamp ? 1000 : 1;
    const min = Number(summary.min) * unit;
    const max = Number(summary.max) * unit;
    if (folded) return `${this.edgeText(min, min, max, timestamp, whole)} – ${this.edgeText(max, min, max, timestamp, whole)}`;
    const mean = summary.mean === null ? '' : ` · mean ${timestamp ? shortDateText(summary.mean * unit) : formatNumber(summary.mean, false)}`;
    return `n ${countText(Number(summary.count))}${mean}`;
  }

  /** An edge on the axis: a date as its year where the range spans years, else its day. */
  private edgeText(v: number, lo: number, hi: number, timestamp: boolean, whole: boolean): string {
    if (!timestamp) return formatNumber(v, whole);
    const years = (hi - lo) / (365.25 * 24 * 3600 * 1e6);
    return years >= 3 ? String(new Date(v / 1000).getUTCFullYear()) : shortDateText(v);
  }

  /** The ramp under the plot: each bin's middle in the colour the map gives it. */
  private ramp(s: Store, bins: Bin[]): TemplateResult {
    const colouring = colouringOf(s);
    const domain = s.get('legend').domains[this.field] ?? {min: bins[0]!.lower, max: bins.at(-1)!.upper};
    const diverging = RAMPS[colouring.ramp].diverging;
    const stops = bins.map((b, i) => `${rgb(colourOfFraction(Math.min(1, Math.max(0, fractionOf((b.lower + b.upper) / 2, domain, colouring.scale, diverging))), colouring.ramp, colouring.reverse))} ${(((i + 0.5) / bins.length) * 100).toFixed(1)}%`);
    return html`<div part="ramp" style=${`background:linear-gradient(to right, ${stops.join(', ')})`}></div>`;
  }

  /**
   * The range a brush covers, its words, and how many matching items lie in it. The bins span every
   * value the viewer may see, so a range reaching the first or the last bin leaves that end open.
   */
  private brushRange(bins: Bin[], brush: Brush, timestamp: boolean, whole: boolean): {gte: number | null; lte: number | null; text: string; count: number} {
    const first = Math.max(0, Math.min(brush.from, brush.to));
    const last = Math.min(bins.length - 1, Math.max(brush.from, brush.to));
    const from = bins[first]!.lower;
    // A bin holds its lower edge and not its upper, so a whole-number or date range ends just
    // below the next bin.
    const end = bins[last]!.upper;
    const to = whole || timestamp ? end - 1 : end;
    const table = this.matchCounts.entry()?.result?.tables[0] ?? this.subjectCounts.entry()?.result?.tables[0];
    const counted = binsOf(table, timestamp);
    const count = counted.slice(first, last + 1).reduce((t, b) => t + b.count, 0);
    const text = timestamp ? dateRangeText(from, to) : `${formatNumber(from, whole)} – ${formatNumber(to, whole)}`;
    return {gte: first === 0 ? null : from, lte: last === bins.length - 1 ? null : to, text, count};
  }

  private applyBrush(verb: ClauseVerb, range: {gte: number | null; lte: number | null}): void {
    this.brush = null;
    this.setClause(verb, {family: 'numeric', gte: range.gte, lte: range.lte});
    void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="plot"]')?.focus());
  }

  /** Which bin a pointer is over. */
  private binAt(e: PointerEvent, n: number): number {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    if (r.width <= 0 || n === 0) return 0;
    return Math.min(n - 1, Math.max(0, Math.floor(((e.clientX - r.left) / r.width) * n)));
  }

  private brushStart(e: PointerEvent, n: number): void {
    if (e.button !== 0 || n === 0) return;
    const i = this.binAt(e, n);
    this.brush = {from: i, to: i, open: false};
    (e.currentTarget as HTMLElement).setPointerCapture?.(e.pointerId);
    e.preventDefault();
  }

  private brushMove(e: PointerEvent, n: number): void {
    if (!this.brush || this.brush.open) return;
    const to = this.binAt(e, n);
    if (to !== this.brush.to) this.brush = {...this.brush, to};
  }

  private brushEnd(e: PointerEvent, n: number): void {
    if (!this.brush || this.brush.open) return;
    this.brush = {...this.brush, to: this.binAt(e, n), open: true};
    void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="brush-filter"]')?.focus());
  }

  /** The arrow keys move a one-bin range, Shift widens it, Enter opens its box and Escape drops it. */
  private brushKey(e: KeyboardEvent, n: number): void {
    if (n === 0) return;
    if (e.key === 'Escape' && this.brush) {
      e.stopPropagation();
      this.brush = null;
      return;
    }
    if (e.key === 'Enter' && this.brush) {
      e.preventDefault();
      this.brush = {...this.brush, open: true};
      void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="brush-filter"]')?.focus());
      return;
    }
    const step = e.key === 'ArrowRight' ? 1 : e.key === 'ArrowLeft' ? -1 : 0;
    if (step === 0) return;
    e.preventDefault();
    const b = this.brush ?? {from: 0, to: 0, open: false};
    const to = Math.min(n - 1, Math.max(0, b.to + (this.brush ? step : 0)));
    this.brush = e.shiftKey ? {from: b.from, to, open: false} : {from: to, to, open: false};
  }

  private onBrushKey = (e: KeyboardEvent): void => {
    if (e.key !== 'Escape') return;
    e.stopPropagation();
    this.brush = null;
    void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="plot"]')?.focus());
  };

  // ---- the colour picker -------------------------------------------------------------------------

  /** The swatch the picker was opened from, which it is placed beside and gives focus back to. */
  private pickedFrom: HTMLElement | null = null;

  private openPicker(e: Event, picking: Picking, colour: string): void {
    this.pickedFrom = e.currentTarget as HTMLElement;
    const c = rgbOfHex(colour) ?? rgbOfHex(hexOf(parseRgb(colour))) ?? [0, 0, 0];
    this.hsv = hsvOf(c);
    this.picking = picking;
    document.addEventListener('pointerdown', this.onOutside, true);
  }

  private closePicker(refocus: boolean): void {
    if (this.picking === null) return;
    this.picking = null;
    document.removeEventListener('pointerdown', this.onOutside, true);
    if (refocus) this.pickedFrom?.focus();
  }

  /** A press outside the picker and its swatch closes it. */
  private onOutside = (e: PointerEvent): void => {
    const path = e.composedPath();
    const pop = this.renderRoot.querySelector('.pop');
    if ((pop && path.includes(pop)) || (this.pickedFrom && path.includes(this.pickedFrom))) return;
    this.closePicker(false);
  };

  /** Give the value being picked `hex`, or its palette colour back where `hex` is null, and report it when `final`. */
  private pick(hex: string | null, final = true): void {
    const s = this.resolvedStore;
    const p = this.picking;
    if (!s || !p) return;
    setColouring(s, {values: withValueColour(colouringOf(s).values, this.field, p.key, hex)});
    if (final) emit(this, 'tessera-valuecolour', {column: this.field, value: p.key, colour: hex});
  }

  /** A colour waiting for the next frame, while a custom colour is dragged. */
  private livePick: {hex: string; frame: number} | null = null;

  /** {@link pick} for a drag: at most once a frame, and the final colour at once. */
  private pickLive(hex: string, final: boolean): void {
    if (this.livePick) cancelAnimationFrame(this.livePick.frame);
    this.livePick = null;
    if (final || typeof requestAnimationFrame === 'undefined') {
      this.pick(hex, final);
      return;
    }
    this.livePick = {
      hex,
      frame: requestAnimationFrame(() => {
        this.livePick = null;
        this.pick(hex, false);
      })
    };
  }

  private colourPicker(s: Store, p: Picking): TemplateResult {
    const colouring = colouringOf(s);
    const legend = s.get('legend');
    const palette = CATEGORY_PALETTES[colouring.palette].colours;
    const code = (legend.categories[this.field] ?? []).find((v) => v.key === p.key)?.code;
    const own = hexOf(colourOfRank(code === undefined ? undefined : legend.ranks[this.field]?.[code], colouring.palette));
    const current = colouring.values[this.field]?.[p.key] ?? own;
    const title = CATEGORY_PALETTES[colouring.palette].title;
    const n = palette.length;
    const choices = [
      ...palette.map((c, i) => ({hex: hexOf(c), label: `${title}, colour ${i + 1} of ${n}`})),
      ...palette.map((c, i) => ({hex: hexOf(lighter(c, LIGHTER)), label: `${title}, lighter colour ${i + 1} of ${n}`}))
    ];
    const [h, sat, val] = this.hsv;
    const custom = hexOf(rgbOfHsv(this.hsv));
    const setHsv = (next: Hsv, final: boolean) => {
      this.hsv = next;
      this.pickLive(hexOf(rgbOfHsv(next)), final);
    };
    const svAt = (e: PointerEvent): Hsv => {
      const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
      const x = r.width > 0 ? Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)) : sat;
      const y = r.height > 0 ? Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)) : 1 - val;
      return [h, x, 1 - y];
    };
    const hueAt = (e: PointerEvent): Hsv => {
      const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
      const run = r.width - HUE_KNOB;
      return [run > 0 ? Math.min(359, Math.max(0, ((e.clientX - r.left - HUE_KNOB / 2) / run) * 360)) : h, sat, val];
    };
    const drag = (read: (e: PointerEvent) => Hsv) => ({
      down: (e: PointerEvent) => {
        (e.currentTarget as HTMLElement).setPointerCapture?.(e.pointerId);
        setHsv(read(e), false);
      },
      move: (e: PointerEvent) => {
        if ((e.currentTarget as HTMLElement).hasPointerCapture?.(e.pointerId)) setHsv(read(e), false);
      },
      up: (e: PointerEvent) => setHsv(read(e), true)
    });
    const sv = drag(svAt);
    const hue = drag(hueAt);
    const clamp = (x: number) => Math.min(1, Math.max(0, x));
    const svKey = (e: KeyboardEvent) => {
      const step = e.shiftKey ? 0.1 : 0.01;
      const next: Hsv | undefined = {
        ArrowLeft: [h, clamp(sat - step), val] as Hsv,
        ArrowRight: [h, clamp(sat + step), val] as Hsv,
        ArrowDown: [h, sat, clamp(val - step)] as Hsv,
        ArrowUp: [h, sat, clamp(val + step)] as Hsv
      }[e.key];
      if (!next) return;
      e.preventDefault();
      setHsv(next, true);
    };
    const hueKey = (e: KeyboardEvent) => {
      const step = e.shiftKey ? 10 : 1;
      const next = {ArrowLeft: h - step, ArrowDown: h - step, ArrowRight: h + step, ArrowUp: h + step, Home: 0, End: 359}[e.key];
      if (next === undefined) return;
      e.preventDefault();
      setHsv([(next + 360) % 360, sat, val], true);
    };
    return html`<div part="colour-popover" class="pop" popover="manual" role="dialog" aria-label=${`Colour of ${p.title}`}
      @keydown=${(e: KeyboardEvent) => {
        if (e.key !== 'Escape') return;
        e.stopPropagation();
        this.closePicker(true);
      }}
      @focusout=${(e: FocusEvent) => {
        const to = e.relatedTarget as Node | null;
        if (to && !(e.currentTarget as HTMLElement).contains(to)) this.closePicker(false);
      }}>
      <div class="top"><span class="t">${p.title}</span><button part="reset" class="quiet" type="button" @click=${() => {
        this.hsv = hsvOf(rgbOfHex(own)!);
        this.pick(null);
      }}>Reset</button></div>
      <div class="choices" role="group" aria-label="Palette colours" style=${`grid-template-columns:repeat(${n}, minmax(0, 1fr))`}>
        ${choices.map(
          ({hex, label}) => html`<button part="choice" type="button" style=${`background:${hex}`} aria-label=${`${label}, ${hex}`} title=${`${label}, ${hex}`}
            aria-pressed=${hex === current ? 'true' : 'false'}
            @click=${() => {
              this.hsv = hsvOf(rgbOfHex(hex)!);
              this.pick(hex);
            }}></button>`
        )}
      </div>
      <div class="hd">Custom</div>
      <div class="custom">
        <div part="sv" role="slider" tabindex="0" aria-label="Saturation and brightness" aria-valuemin="0" aria-valuemax="100" aria-valuenow=${Math.round(sat * 100)}
          aria-valuetext=${`Saturation ${Math.round(sat * 100)}%, brightness ${Math.round(val * 100)}%`}
          style=${`background:linear-gradient(to top, #000000, rgba(0, 0, 0, 0)), linear-gradient(to right, #ffffff, hsl(${h.toFixed(0)}, 100%, 50%))`}
          @pointerdown=${sv.down} @pointermove=${sv.move} @pointerup=${sv.up} @keydown=${svKey}>
          <span class="knob" style=${`left:${(sat * 100).toFixed(1)}%;top:${((1 - val) * 100).toFixed(1)}%`}></span>
        </div>
        <div part="hue" role="slider" tabindex="0" aria-label="Hue" aria-valuemin="0" aria-valuemax="359" aria-valuenow=${Math.round(h)}
          @pointerdown=${hue.down} @pointermove=${hue.move} @pointerup=${hue.up} @keydown=${hueKey}>
          <span class="knob" style=${`left:calc(${HUE_KNOB / 2}px + (100% - ${HUE_KNOB}px) * ${(h / 360).toFixed(4)})`}></span>
        </div>
        <label>Hex<input part="hex" type="text" spellcheck="false" .value=${custom.toUpperCase()}
          @change=${(e: Event) => {
            const input = e.target as HTMLInputElement;
            const text = input.value.trim();
            const c = rgbOfHex(text.startsWith('#') ? text : `#${text}`);
            if (!c) {
              input.value = custom.toUpperCase();
              return;
            }
            this.hsv = hsvOf(c);
            this.pick(hexOf(c));
          }} /></label>
      </div>
    </div>`;
  }

  /** Show the picker in the top layer, right of the card and level with its swatch; focus goes into it as it opens. */
  private placePicker(changed: PropertyValues<this>): void {
    const pop = this.renderRoot.querySelector<HTMLElement>('.pop');
    const from = this.pickedFrom;
    if (!pop || !from) return;
    if (typeof pop.showPopover === 'function' && !pop.matches(':popover-open')) {
      try {
        pop.showPopover();
      } catch {
        // Shown already; the picker is in the page either way.
      }
    }
    const a = from.getBoundingClientRect();
    const own = this.getBoundingClientRect();
    const width = pop.offsetWidth || 264;
    const height = pop.offsetHeight || 0;
    const vw = typeof innerWidth === 'number' ? innerWidth : 1024;
    const vh = typeof innerHeight === 'number' ? innerHeight : 768;
    let left = own.right + 12;
    if (left + width > vw - 8) left = own.left - width - 12;
    pop.style.left = `${Math.round(Math.max(8, Math.min(left, vw - width - 8)))}px`;
    pop.style.top = `${Math.round(Math.max(8, Math.min(a.top - 8, vh - height - 8)))}px`;
    if (changed.has('picking') && changed.get('picking') === null) (pop.querySelector<HTMLElement>('[aria-pressed="true"]') ?? pop.querySelector<HTMLElement>('button'))?.focus();
  }
}

/** `rgb(r, g, b)` as RGB, for a swatch colour that is not a hex string. */
function parseRgb(text: string): [number, number, number] {
  const m = /rgba?\((\d+),\s*(\d+),\s*(\d+)/.exec(text);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : [0, 0, 0];
}

attachContextRoot();
defineOnce('tessera-field-card', TesseraFieldCard);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-field-card': TesseraFieldCard;
  }
}
