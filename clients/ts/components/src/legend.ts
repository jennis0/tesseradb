import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {CLUSTER_PREFIX, artifactName, colourLayers, composeFilters, type CategoryValue, type ClauseVerb, type FilterDraft, type Masked, type Meta, type Rgba, type Store} from '@tesseradb/client';
import {NEUTRAL} from '@tesseradb/client/internal';
import {CATEGORY_PALETTES, RAMPS, type CategoryPaletteName, type Colouring, type RampName, type RampScale} from '@tesseradb/deck';
import {
  UNMAPPED,
  clusterLayerOf,
  colourOfFraction,
  colourOfRank,
  css as rgb,
  fractionOf,
  hexOf,
  lighter,
  paletteValues,
  rgbOfHex,
  valueAtFraction
} from '@tesseradb/deck/internal';
import {TesseraElement, UNNAMED, columnCaption, dateText, emit} from './base.js';
import {colouringOf, setColouring, watchColouring, withValueColour} from './colouring.js';
import {radioKeys} from './display.js';
import {hsvOf, rgbOfHsv, type Hsv} from './hsv.js';
import {icon} from './icons.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/** The entries shown where `limit` is 0, as many as a card can hold. */
const ENTRIES_SHOWN = 40;

/** The longest name, in characters, that a row in two columns shows whole at 12 px in a 300 px card. */
const SHORT_NAME = 20;
/** The characters a row gives up for each pressed × at its end. */
const PER_DISMISS = 4;

const graphemes = new Intl.Segmenter('en', {granularity: 'grapheme'});
/** A name's length as a reader counts it: characters, not UTF-16 units. */
const lengthOf = (text: string): number => [...graphemes.segment(text)].length;

/** How far each lighter colour in the colour picker is taken towards white. */
const LIGHTER = 0.45;

type Column = Meta['declaredScalars'][number];

/** A choice in Colour by: its value for `setColourBy` (with `@level` for one level of a layer), its title and its kind. */
type ColourOption = {value: string; title: string; kind: string; checked: boolean; cluster: boolean};

/** The value whose colour the picker is changing. */
type Picking = {column: string; key: string; title: string};

/**
 * What the colours mean, under a "Colour" heading. For a category column, one row per value the
 * points on screen carry, in its colour; for a number column, a ramp over the range of the points
 * served, with its lowest and highest values; under colour by cluster, the served artifacts in
 * their colours. The first `limit` rows show, 40 where `limit` is 0, with an "N more" button that
 * shows the rest until the colouring changes.
 *
 * The rows sit in two columns while every name shown is short, and in one otherwise. A category
 * row is a swatch, the value's name, Highlight and Filter buttons, and a count where the store
 * holds exact counts for the column (not built yet: no route serves them, so no count shows). The
 * buttons show over the row's end while it is hovered or focused. A pressed button stays shown as
 * a ×, which takes the value out of that clause, and its row is filled: grey for the filter, the
 * highlight colour for the highlight. Filter adds the value to the column's filter, which may hold
 * several; the rows left out empty their swatch.
 * Highlight picks the value out without changing any count; the other rows grey. The two are
 * separate clauses and a row can have both pressed: filtered to two values with one highlighted,
 * the map shows the two and picks out the one. Both go through the store's filters, so the chips
 * are the ones `<tessera-filter-panel>` shows. The buttons appear only where `meta` offers the
 * column's `in` operator.
 *
 * The swatch opens a colour picker: the palette's colours and a lighter row, a custom area with a
 * hue bar and a hex field, and Reset, which gives the value its palette colour back. A choice
 * applies at once and fires `tessera-valuecolour`. A number column's ramp carries a range with two
 * handles, where the column can be filtered by range: dragging across the ramp or moving a handle
 * (with the arrow keys, too) sets the column's range filter, and the range follows the filter's
 * chip.
 *
 * `selectable` puts the *Colour by* choice in the heading, a button opening a menu of the rendered
 * columns and every layer that can colour, drawn or not; colouring by a layer does not draw it. For
 * a category column the menu offers the palettes, and for a number column the ramps, a linear or
 * log scale and reversal, unless `hide-palettes` is set. The colour choices are shared with every
 * map reading the same store. A *Level* choice appears in the heading after Colour by, under colour
 * by a levelled layer with several levels served. It shows the name of the option chosen, a level
 * title or the automatic choice, and lists the rest. The Colour by name keeps its room: where the two do not fit
 * on one line, the Level choice moves to a line of its own under the heading, and only then is the
 * name cut short, with the whole name as its tooltip. Which layers are drawn is
 * `<tessera-layer-picker>`'s.
 *
 * @summary What the map's colours mean, and the colour controls.
 * @tagname tessera-legend
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-colourchange']>} tessera-colourchange - The Colour
 *   by choice changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-levelchange']>} tessera-levelchange - The Level
 *   choice, or a levelled layer's entry in Colour by, chose a level.
 * @fires {CustomEvent<TesseraEventDetails['tessera-valuecolour']>} tessera-valuecolour - A value's
 *   colour was chosen or reset in the colour picker.
 * @fires {CustomEvent<TesseraEventDetails['tessera-palettechange']>} tessera-palettechange - The
 *   palette, the ramp, its scale or its direction was chosen in Colour by.
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A row's
 *   Highlight or Filter button, or the ramp's range, changed the column's clause.
 * @csspart title - The heading, holding the Colour by button under `selectable`.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - "Values unavailable" where the names were refused, or "Column unavailable"
 *   where the column is not rendered, with `data-code` where the server gave one.
 * @csspart colour-by - The Colour by button, naming what the points are coloured by.
 * @csspart colour-menu - The Colour by menu, while it is open.
 * @csspart option - An entry in the Colour by menu, with `data-value`, `aria-checked`, and
 *   `data-kind`: `layer` for a layer or one level of a levelled layer, `category`, `number` or
 *   `none`.
 * @csspart palette - A palette in the Colour by menu, with `data-palette` and `aria-checked`.
 * @csspart ramp-option - A ramp in the Colour by menu, with `data-ramp` and `aria-checked`.
 * @csspart scale - The Linear and Log choice in the Colour by menu.
 * @csspart reverse - The Reverse switch in the Colour by menu.
 * @csspart level - The Level choice: its short label and the select over it.
 * @csspart level-select - The Level select.
 * @csspart swatches - The list of rows, with `data-columns` set to `1` or `2`.
 * @csspart entry - One row, with `data-key` for a category value and `data-state`: `out` for a
 *   value the column's filter leaves out; for one it keeps, `lit` or `dim` while the column is
 *   highlighted, else `filtered` while it is filtered; else empty. `data-clause` names the clauses
 *   the value itself is in: `filter`, `highlight`, or both separated by a space.
 * @csspart swatch - A row's colour: for a category value, the button that opens the colour picker.
 * @csspart name - A row's name, with `data-unnamed` on a cluster that has none.
 * @csspart highlight - A row's Highlight button, with `aria-pressed`; drawn as a × while pressed.
 * @csspart filter - A row's Filter button, with `aria-pressed`; drawn as a × while pressed.
 * @csspart count - A row's exact count, where the store holds one.
 * @csspart more - The "N more" button, where there are more entries than show.
 * @csspart ramp - A number column's ramp.
 * @csspart range - The span of the ramp the column's range filter keeps, while one is set.
 * @csspart range-low - The handle of the range's low end, a slider.
 * @csspart range-high - The handle of the range's high end, a slider.
 * @csspart value - The ramp's lowest and highest values, beneath its ends.
 * @csspart range-value - The range's ends, beneath the ramp, while a range is set.
 * @csspart colour-popover - The colour picker, while it is open.
 * @csspart choice - A colour in the picker's palette rows, with `aria-pressed`.
 * @csspart sv - The picker's saturation and brightness area, a slider in two directions.
 * @csspart hue - The picker's hue bar, a slider.
 * @csspart hex - The picker's hex field.
 * @csspart reset - The picker's Reset button.
 */
export class TesseraLegend extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      /* The heading wraps only to move the Level choice under it; the lead keeps Colour by on the first line. */
      [part='title'] {
        margin-bottom: 10px;
        flex-wrap: wrap;
        justify-content: flex-start;
        column-gap: 12px;
        row-gap: 2px;
      }
      [part='title'] .lead {
        display: flex;
        align-items: center;
        gap: 12px;
        flex: 1 1 auto;
        min-width: 0;
      }
      [part='colour-by'] {
        display: inline-flex;
        align-items: center;
        gap: 4px;
        min-width: 0;
        margin-left: auto;
        font-size: 12px;
        font-weight: 500;
        letter-spacing: 0;
        text-transform: none;
        color: var(--_tessera-ink);
      }
      [part='colour-by'] .t {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      /* The Level choice: a short label, with the select over it taking the clicks and keys. */
      [part='level'] {
        flex: 0 0 auto;
        font-size: 12px;
        font-weight: 400;
        letter-spacing: 0;
        text-transform: none;
        color: var(--_tessera-ink-2);
        border-radius: var(--_tessera-radius-control);
      }
      [part='level'] .t {
        padding-right: 18px;
        white-space: nowrap;
      }
      [part='level'] select {
        position: absolute;
        inset: 0;
        width: 100%;
        opacity: 0;
      }
      [part='level']:focus-within {
        outline: 2px solid var(--_tessera-accent);
        outline-offset: 1px;
      }
      [part='swatches'] {
        display: grid;
        grid-template-columns: minmax(0, 1fr);
        row-gap: 1px;
        max-height: 340px;
        overflow-y: auto;
        margin: 0 -4px;
        font-size: 12px;
      }
      [part='swatches'][data-columns='2'] {
        grid-template-columns: repeat(2, minmax(0, 1fr));
        column-gap: 4px;
        row-gap: 2px;
      }
      [part~='entry'] {
        position: relative;
        display: grid;
        grid-template-columns: 10px minmax(0, 1fr);
        align-items: center;
        column-gap: 8px;
        min-height: 24px;
        padding: 0 4px;
        border-radius: 5px;
      }
      [data-columns='2'] [part~='entry'] {
        column-gap: 7px;
        min-height: 20px;
      }
      [part='swatches'].counted [part~='entry'] {
        grid-template-columns: 10px minmax(0, 1fr) 72px;
      }
      [part~='entry']:hover,
      [part~='entry']:focus-within,
      [part~='entry'][data-clause~='filter'] {
        background: var(--_tessera-surface-2);
      }
      [part~='entry'][data-clause] {
        font-weight: 500;
      }
      [part~='entry'][data-clause~='highlight'],
      [part~='entry'][data-state='lit'] {
        background: var(--_tessera-highlight-soft);
        color: var(--_tessera-highlight);
      }
      /* Room at the end of a row for its pressed buttons, which sit over it. */
      [part~='entry'][data-clause='filter'],
      [part~='entry'][data-clause='highlight'] {
        padding-right: 26px;
      }
      [part~='entry'][data-clause='filter highlight'] {
        padding-right: 48px;
      }
      [part='swatch'] {
        --c: transparent;
        display: inline-block;
        width: 10px;
        height: 10px;
        border-radius: 2px;
        background: var(--c);
      }
      button[part='swatch'] {
        cursor: pointer;
      }
      [data-state='out'] [part='swatch'] {
        background: transparent;
        box-shadow: inset 0 0 0 1.5px var(--c);
      }
      [data-state='dim'] [part='swatch'] {
        background: color-mix(in srgb, var(--_tessera-ink-3) 30%, var(--_tessera-surface));
      }
      [part='name'] {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      [data-state='out'] [part='name'],
      [data-state='dim'] [part='name'] {
        color: var(--_tessera-ink-3);
      }
      /* The buttons sit over the row's end, on the row's own fill, so showing them moves nothing. */
      .verbs {
        position: absolute;
        top: 50%;
        right: 2px;
        transform: translateY(-50%);
        display: flex;
        gap: 2px;
        padding-left: 4px;
        border-radius: 5px;
        background: inherit;
      }
      .verbs button {
        width: 20px;
        height: 20px;
        display: grid;
        place-items: center;
        border-radius: 4px;
        color: var(--_tessera-ink-2);
      }
      .verbs button[aria-pressed='false'] {
        display: none;
      }
      [part~='entry']:hover .verbs button,
      [part~='entry']:focus-within .verbs button {
        display: grid;
      }
      .verbs button:hover {
        background: var(--_tessera-surface-3);
      }
      [part='highlight'][aria-pressed='true'] {
        color: var(--_tessera-highlight);
      }
      [part='count'] {
        text-align: right;
        font-size: 12px;
        font-variant-numeric: tabular-nums;
        color: var(--_tessera-ink-2);
      }
      [data-state='lit'] [part='count'] {
        color: var(--_tessera-highlight);
      }
      [data-state='out'] [part='count'],
      [data-state='dim'] [part='count'] {
        color: var(--_tessera-ink-3);
      }
      [part='more'] {
        margin-top: 8px;
      }
      /* A number column: the ramp, the range on it, and the values beneath. */
      .track {
        position: relative;
        height: 22px;
        margin: 2px 0 0;
        touch-action: none;
      }
      .track.filterable {
        cursor: crosshair;
      }
      [part='ramp'] {
        position: absolute;
        left: 0;
        right: 0;
        top: 6px;
        height: 10px;
        border-radius: 2px;
      }
      [part='range'] {
        position: absolute;
        top: 0;
        bottom: 0;
        background: color-mix(in srgb, var(--_tessera-ink) 6%, transparent);
        border-left: 1.5px solid var(--_tessera-ink);
        border-right: 1.5px solid var(--_tessera-ink);
        pointer-events: none;
      }
      [part='range-low'],
      [part='range-high'] {
        position: absolute;
        top: 1px;
        width: 8px;
        height: 20px;
        margin-left: -4px;
        border: 1.5px solid var(--_tessera-ink);
        border-radius: 3px;
        background: var(--_tessera-surface);
        cursor: ew-resize;
      }
      .axis {
        position: relative;
        height: 18px;
        margin-top: 2px;
        font-size: 12px;
        color: var(--_tessera-ink-2);
        font-variant-numeric: tabular-nums;
        white-space: nowrap;
      }
      .axis > * {
        position: absolute;
      }
      .axis .lo {
        left: 0;
      }
      .axis .hi {
        right: 0;
      }
      [part='range-value'] {
        color: var(--_tessera-ink);
        font-weight: 500;
      }
      /* The popovers: the Colour by menu and the colour picker, in the top layer. */
      .pop {
        position: fixed;
        inset: auto;
        margin: 0;
        padding: 0;
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
      .menu {
        width: 280px;
        padding: 6px 0;
      }
      .menu .hd {
        margin: 0;
        padding: 6px 14px 4px;
      }
      .menu .hd.rule {
        margin-top: 6px;
        padding-top: 10px;
        border-top: 1px solid var(--_tessera-line-2);
      }
      .menu [role='radio'] {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 12px;
        width: 100%;
        padding: 7px 14px;
        text-align: left;
      }
      .menu [role='radio']:hover,
      .menu [role='radio'][aria-checked='true'] {
        background: var(--_tessera-surface-2);
      }
      .pop [role='radio']:focus-visible,
      .pop button:focus-visible {
        outline-offset: -2px;
      }
      .menu [part~='option'][aria-checked='true'] {
        font-weight: 500;
      }
      .menu .kind {
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
      .menu [part='palette'],
      .menu [part='ramp-option'] {
        flex-direction: column;
        align-items: flex-start;
        gap: 5px;
        padding: 9px 14px;
      }
      .menu .pt {
        display: flex;
        align-items: center;
        gap: 6px;
      }
      .menu .tag {
        padding: 0 5px;
        border: 1px solid var(--_tessera-line);
        border-radius: 4px;
        font-size: 11px;
        color: var(--_tessera-ink-2);
      }
      .menu .strip {
        display: flex;
        gap: 2px;
      }
      .menu .strip span {
        width: 18px;
        height: 10px;
        border-radius: 2px;
      }
      .menu .bar {
        display: block;
        width: 100%;
        height: 10px;
        border-radius: 2px;
      }
      .menu .opts {
        display: grid;
        grid-template-columns: 80px minmax(0, 1fr);
        align-items: center;
        row-gap: 8px;
        margin-top: 6px;
        padding: 10px 14px 6px;
        border-top: 1px solid var(--_tessera-line-2);
        font-size: 12px;
        color: var(--_tessera-ink-2);
      }
      .seg2 {
        display: inline-flex;
        justify-self: start;
        padding: 2px;
        border-radius: 7px;
        background: var(--_tessera-surface-3);
      }
      .menu .seg2 [role='radio'] {
        width: auto;
        padding: 3px 10px;
        border-radius: 5px;
        font-size: 12px;
        color: var(--_tessera-ink-2);
      }
      .menu .seg2 [role='radio'][aria-checked='true'] {
        background: var(--_tessera-surface);
        box-shadow: 0 1px 2px rgba(0, 0, 0, 0.1);
        color: var(--_tessera-ink);
        font-weight: 500;
      }
      .menu .opts .switch {
        justify-self: start;
      }
      .picker {
        width: 264px;
      }
      .picker .head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 8px;
        padding: 12px 14px 8px;
      }
      .picker .head .t {
        font-weight: 600;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      .picker .choices {
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
      .picker .hd {
        margin: 0;
        padding: 10px 14px 4px;
        border-top: 1px solid var(--_tessera-line-2);
      }
      .picker .custom {
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
      .picker label {
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

  /** Puts the Colour by choice in the heading. */
  @property({type: Boolean}) accessor selectable = false;
  /** Under `selectable`, also renders the rows or the ramp below the heading. */
  @property({type: Boolean}) accessor readout = false;
  /** How many entries show before "N more" offers the rest; 0 shows the first 40. */
  @property({type: Number}) accessor limit = 0;
  /** Leaves the palette and ramp choices out of the Colour by menu, for a host that sets them. */
  @property({type: Boolean, attribute: 'hide-palettes'}) accessor hidePalettes = false;
  /** Whether "N more" was pressed, for the colouring it was pressed under. @internal */
  @state() accessor expanded = false;
  /** Whether the Colour by menu is open. @internal */
  @state() accessor menuOpen = false;
  /** The value the colour picker is open for. @internal */
  @state() accessor picking: Picking | null = null;
  /** The colour picker's custom colour. @internal */
  @state() accessor hsv: Hsv = [0, 0, 0];
  /** A range being dragged on the ramp, as fractions of it, before it is sent. @internal */
  @state() accessor dragging: {low: number; high: number} | null = null;
  private expandedFor: string | null = null;
  private unwatchColouring: (() => void) | null = null;
  /** Where a drag on the ramp started, and which end it moves. */
  private dragFrom: {
    anchor: number;
    /** The anchored end's own bound when a handle is moved; unset for a new range. */
    keep: number | null | undefined;
    toBound: (t: number) => number | null;
    commit: (gte: number | null, lte: number | null) => void;
    /** Where the pointer went down, from 0 to 1. */
    start: number;
  } | null = null;

  protected override onStoreAdopted(store: Store | null): void {
    this.unwatchColouring?.();
    this.unwatchColouring = store ? watchColouring(store, () => this.requestUpdate()) : null;
  }

  protected override resetServerData(): void {
    // The picker and the menu name values and layers the server answered.
    this.closePopovers(false);
    this.dragging = null;
    this.dragFrom = null;
  }

  override disconnectedCallback(): void {
    this.closePopovers(false);
    if (this.following) cancelAnimationFrame(this.following.frame);
    this.following = null;
    if (this.livePick) cancelAnimationFrame(this.livePick.frame);
    this.livePick = null;
    document.removeEventListener('pointerdown', this.onOutside, true);
    super.disconnectedCallback();
  }

  protected override onStoreChange(): void {
    // A new colouring is a new list, which starts cut again.
    const colourBy = this.resolvedStore?.get('legend').colourBy ?? null;
    if (colourBy !== this.expandedFor) {
      this.expandedFor = colourBy;
      this.expanded = false;
      this.picking = null;
    }
    super.onStoreChange();
  }

  private choose(value: string): void {
    const s = this.resolvedStore;
    if (!s) return;
    // `cluster:<layer>@<level>` chooses a levelled layer's colouring and its level together.
    const at = value.lastIndexOf('@');
    const chosen = value === '' ? null : at > 0 ? value.slice(0, at) : value;
    s.setColourBy(chosen);
    emit(this, 'tessera-colourchange', {colourBy: chosen});
    if (at > 0) this.chooseLevel(value.slice(at + 1));
  }

  /** The level chosen in the Level select; `null` chooses the level drawn by default. */
  @property({type: Number, attribute: 'cluster-level'}) accessor level: number | null = null;
  /** The level drawn when none is chosen, named in the Level select's first entry; `null` names the deepest served. */
  @property({type: Number, attribute: false}) accessor autoLevel: number | null = null;

  private chooseLevel(value: string): void {
    const level = value === '' ? null : Number(value);
    this.level = level;
    emit(this, 'tessera-levelchange', {level});
  }

  /** Change the palette, the ramp, its scale or its direction, and report all four. */
  private choosePalette(patch: Partial<Pick<Colouring, 'palette' | 'ramp' | 'scale' | 'reverse'>>): void {
    const s = this.resolvedStore;
    if (!s) return;
    setColouring(s, patch);
    const {palette, ramp, scale, reverse} = colouringOf(s);
    emit(this, 'tessera-palettechange', {palette, ramp, scale, reverse});
  }

  /**
   * Put `key` in or out of `column`'s clause in the position `verb`. The clause in the other
   * position is kept.
   */
  private applyVerb(column: string, key: string, verb: ClauseVerb): void {
    const s = this.resolvedStore;
    if (!s) return;
    if (!this.offers(column, 'category', 'in')) return;
    const draft = s.get('filters').draft;
    const held = draft[verb][column];
    const was = held?.family === 'category' ? held.keys : [];
    const keys = was.includes(key) ? was.filter((k) => k !== key) : [...was, key];
    const next: FilterDraft = {...draft, [verb]: {...draft[verb], [column]: {family: 'category', keys}}};
    s.setFilters(next);
    emit(this, 'tessera-filterchange', {column, verb, expr: composeFilters(next, verb)});
  }

  /** Whether `meta` offers `op` on `column` as a column of `family`. */
  private offers(column: string, family: 'category' | 'numeric', op: 'in' | 'range'): boolean {
    return this.resolvedStore?.get('meta')?.filterOperands.some((o) => o.column === column && o.family === family && o.operands.includes(op)) ?? false;
  }

  /**
   * Set `column`'s range filter to `[gte, lte]` in the column's units, rounded as the column stores
   * them. A `null` end is open: the ramp spans only the values drawn, which are a sample, so an end
   * dragged to the ramp's edge sets no bound there.
   */
  private applyRange(column: Column, gte: number | null, lte: number | null): void {
    const s = this.resolvedStore;
    if (!s) return;
    if (!this.offers(column.name, 'numeric', 'range')) return;
    const draft = s.get('filters').draft;
    const whole = column.arrowType !== 'f32' && column.arrowType !== 'f64';
    const round = (v: number | null) => (v === null ? null : whole ? Math.round(v) : Number(v.toPrecision(6)));
    const [lo, hi] = gte !== null && lte !== null && gte > lte ? [lte, gte] : [gte, lte];
    const next: FilterDraft = {...draft, filter: {...draft.filter, [column.name]: {family: 'numeric', gte: round(lo), lte: round(hi)}}};
    s.setFilters(next);
    emit(this, 'tessera-filterchange', {column: column.name, expr: composeFilters(next)});
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel"><h2 part="title">Colour</h2>${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const legend = s.get('legend');
    const artifacts = s.get('artifacts');
    const colouring = colouringOf(s);
    const columns = meta.declaredScalars.filter((c) => c.render);
    const colourBy = legend.colourBy;
    // The level options are the rungs present among the colouring layer's served artifacts,
    // titled from `meta.levels` where the layer is levelled, else "Level N". The server computes
    // `rung` per layer kind, so a tree's depths are offered too.
    const cluster = clusterLayerOf(colourBy);
    const clusterMeta = cluster ? meta.layers.find((l) => l.name === cluster) : null;
    const rungs = new Set<number>();
    if (cluster) {
      for (const x of artifacts.colourServed) rungs.add(x.rung);
    }
    const levelsServed = [...rungs].sort((x, y) => x - y);
    const levelTitle = (l: number) => clusterMeta?.levels.find((x) => x.level === l)?.title ?? `Level ${l}`;
    // The level drawn: the one chosen, else the explorer's, else the deepest served.
    const drawnLevel = this.level ?? this.autoLevel ?? levelsServed.at(-1) ?? null;
    const options: ColourOption[] = [
      {value: '', title: 'None', kind: '', checked: colourBy === null, cluster: false},
      ...colourLayers(meta.layers).flatMap((decl): ColourOption[] => {
        // A levelled layer is offered once per level, by the declared level titles, since
        // colouring it is membership at one level. Any other layer is offered once.
        const value = `${CLUSTER_PREFIX}${decl.name}`;
        if (decl.levels.length === 0) return [{value, title: decl.title || decl.name, kind: 'Layer', checked: colourBy === value, cluster: true}];
        return decl.levels.map((lv) => ({
          value: `${value}@${lv.level}`,
          title: lv.title || `Level ${lv.level}`,
          kind: 'Layer',
          checked: colourBy === value && (drawnLevel ?? decl.levels.at(-1)!.level) === lv.level,
          cluster: true
        }));
      }),
      ...columns.map((c) => ({value: c.name, title: columnCaption(c.name), kind: c.category ? 'Category' : 'Number', checked: colourBy === c.name, cluster: false}))
    ];
    const current = options.find((o) => o.checked);
    const colourByButton = this.selectable
      ? html`<button part="colour-by" type="button" aria-haspopup="dialog" aria-expanded=${this.menuOpen ? 'true' : 'false'} aria-label=${`Colour by: ${current?.title ?? 'None'}`}
          @click=${() => {
            // One popover at a time: the menu takes the picker's place.
            this.picking = null;
            this.menuOpen = !this.menuOpen;
          }}><span class="t" title=${current?.title ?? 'None'}>${current?.title ?? 'None'}</span>${icon('chev', 12, 1.4)}</button>`
      : nothing;
    const column = columns.find((c) => c.name === colourBy) ?? null;
    const menu = this.selectable && this.menuOpen ? this.colourMenu(options, column, colouring) : nothing;
    const automatic = this.autoLevel === null ? 'Deepest level' : `Automatic (${levelTitle(this.autoLevel)})`;
    const levelSelect =
      this.selectable && cluster && levelsServed.length > 1
        ? html`<span part="level" class="choice"><span class="t" aria-hidden="true">${this.level === null ? automatic : levelTitle(this.level)}</span>
            <select part="level-select" aria-label="Level" @change=${(e: Event) => this.chooseLevel((e.target as HTMLSelectElement).value)}>
              <option value="" ?selected=${this.level === null}>${automatic}</option>
              ${levelsServed.map((l) => html`<option value=${l} ?selected=${this.level === l}>${levelTitle(l)}</option>`)}
            </select>${icon('chev', 12, 1.4)}</span>`
        : nothing;
    const heading = html`<h2 part="title"><span class="lead">Colour${colourByButton}</span>${levelSelect}</h2>`;
    const wrap = (body: unknown) => html`<div class="panel">${heading}${menu}${body}${this.picking ? this.colourPicker(this.picking, colouring, legend.ranks[this.picking.column] ?? {}, legend.categories[this.picking.column] ?? []) : nothing}</div>`;
    if (!this.readout && this.selectable) return wrap(html`<span part="state" data-state="shown"></span>`);
    if (colourBy === null) return wrap(html`<span part="state" data-state="shown"></span>`);

    const plain = (c: Rgba | readonly number[], text: string, title = text, unnamed = false) =>
      html`<div part="entry" role="listitem"><span part="swatch" style=${`--c:${rgb(c as Rgba)}`}></span><span part="name" title=${title} ?data-unnamed=${unnamed}>${text}</span></div>`;
    if (cluster) {
      const named = artifacts.colourServed;
      return wrap(html`<span part="state" data-state="shown"></span>${this.rows(
        [
          ...named.map((a) => {
            const name = artifactName(a, artifacts.attached);
            return plain(artifacts.colours.get(artifacts.table.ordinalOf(a.layer, a.tesseraId)) ?? NEUTRAL, name ?? UNNAMED, name ?? UNNAMED, name === null);
          }),
          plain(NEUTRAL, 'Not yet coloured')
        ],
        false,
        [...named.map((a) => artifactName(a, artifacts.attached) ?? UNNAMED), 'Not yet coloured'].map((name) => ({name, pressed: 0}))
      )}`);
    }
    if (!column) return wrap(html`<span part="state" data-state="refused"><span class="dot refuse"></span><span part="refusal">Column unavailable</span></span>`);
    const error = legend.categoryErrors[column.name];
    if (error) return wrap(html`<span part="state" data-state="refused"><span class="dot refuse"></span><span part="refusal" data-code=${error.code}>Values unavailable</span></span>`);
    if (column.category) {
      const values = legend.categories[column.name];
      if (!values) return wrap(renderState('loading', s.get('status')));
      return wrap(html`<span part="state" data-state="shown"></span>${this.categoryRows(s, column.name, values, legend.ranks[column.name] ?? {}, colouring, legend.counts?.[column.name])}`);
    }
    const domain = legend.domains[column.name];
    if (!domain) return wrap(html`<span part="state" data-state="empty">Nothing in view</span>`);
    return wrap(html`<span part="state" data-state="shown"></span>${this.numeric(s, column, domain, colouring)}`);
  }

  /** The rows of a category column: swatch, name, the two verbs and the count. */
  private categoryRows(s: Store, column: string, values: CategoryValue[], ranks: Record<number, number>, colouring: Colouring, counts: Record<string, Masked> | undefined): TemplateResult {
    const meta = s.get('meta');
    const {draft} = s.get('filters');
    const filterable = this.offers(column, 'category', 'in');
    const keysIn = (verb: ClauseVerb) => {
      const held = draft[verb][column];
      return held?.family === 'category' ? held.keys : [];
    };
    const filtered = keysIn('filter');
    const lit = keysIn('highlight');
    const has = (keys: string[], key: string | null) => key !== null && keys.includes(key);
    const stateOfKey = (key: string | null) =>
      filtered.length > 0 && !has(filtered, key) ? 'out' : lit.length > 0 ? (has(lit, key) ? 'lit' : 'dim') : filtered.length > 0 ? 'filtered' : '';
    const chosen = colouring.values[column] ?? {};
    const shown = paletteValues(values, ranks, colouring.palette);
    const counted = counts !== undefined;
    const row = ({value, rank}: {value: CategoryValue; rank: number}) => {
      const title = value.title ?? value.key;
      const colour = chosen[value.key] ?? rgb(colourOfRank(rank, colouring.palette));
      const count = counts?.[value.key];
      const onFilter = filtered.includes(value.key);
      const onLit = lit.includes(value.key);
      const clause = [onFilter ? 'filter' : '', onLit ? 'highlight' : ''].filter(Boolean).join(' ');
      return html`<div part="entry" role="listitem" data-key=${value.key} data-state=${stateOfKey(value.key)} data-clause=${clause || nothing}>
        <button part="swatch" type="button" style=${`--c:${colour}`} aria-haspopup="dialog" aria-label=${`Change colour of ${title}`}
          @click=${() => this.openPicker({column, key: value.key, title}, colour)}></button>
        <span part="name" title=${value.key}>${title}</span>
        ${counted ? html`<span part="count">${count?.exact ? count.value.toLocaleString('en-GB') : ''}</span>` : nothing}
        ${filterable
          ? html`<span class="verbs">
              <button part="highlight" type="button" aria-pressed=${onLit ? 'true' : 'false'} aria-label=${onLit ? `Stop highlighting ${title}` : `Highlight ${title}`}
                title=${onLit ? 'Stop highlighting' : 'Highlight'} @click=${() => this.applyVerb(column, value.key, 'highlight')}>${onLit ? icon('close', 12) : icon('highlight', 13)}</button>
              <button part="filter" type="button" aria-pressed=${onFilter ? 'true' : 'false'} aria-label=${onFilter ? `Stop filtering to ${title}` : `Filter to ${title}`}
                title=${onFilter ? 'Stop filtering' : 'Filter'} @click=${() => this.applyVerb(column, value.key, 'filter')}>${onFilter ? icon('close', 12) : icon('filter', 13, 1.5)}</button>
            </span>`
          : nothing}
      </div>`;
    };
    // A value past the palette's end with a colour chosen for it has a row of its own; the rest
    // share one colour, named once, where there are any.
    const size = CATEGORY_PALETTES[colouring.palette].colours.length;
    const past = values.map((value) => ({value, rank: ranks[value.code] ?? Number.MAX_SAFE_INTEGER})).filter((v) => v.rank >= size);
    const pastChosen = past.filter((v) => chosen[v.value.key] !== undefined).sort((a, b) => a.rank - b.rank);
    const other =
      past.length > pastChosen.length
        ? [html`<div part="entry" role="listitem" data-state=${stateOfKey(null)}><span part="swatch" style=${`--c:${rgb(UNMAPPED)}`}></span><span part="name">Other</span></div>`]
        : [];
    // Each pressed × takes room from its row's name.
    const names = [...shown, ...pastChosen].map(({value}) => ({name: value.title ?? value.key, pressed: Number(filtered.includes(value.key)) + Number(lit.includes(value.key))}));
    return this.rows([...shown.map(row), ...pastChosen.map(row), ...other], counted, other.length > 0 ? [...names, {name: 'Other', pressed: 0}] : names);
  }

  /**
   * The rows, the first of them until "N more" is pressed, in two columns where every name shown
   * is short enough to fit one and no count takes a row's end.
   */
  private rows(entries: TemplateResult[], counted: boolean, names: {name: string; pressed: number}[]): TemplateResult {
    const first = this.limit > 0 ? this.limit : ENTRIES_SHOWN;
    const cut = !this.expanded && entries.length > first;
    const shown = cut ? entries.slice(0, first) : entries;
    const columns = !counted && names.slice(0, shown.length).every((n) => lengthOf(n.name) <= SHORT_NAME - PER_DISMISS * n.pressed) ? 2 : 1;
    return html`<div part="swatches" role="list" class=${counted ? 'counted' : ''} data-columns=${columns}>${shown}</div>${cut
      ? html`<button part="more" class="more-link" type="button" @click=${() => (this.expanded = true)}>${(entries.length - first).toLocaleString('en-GB')} more</button>`
      : nothing}`;
  }

  /**
   * A number column's ramp, with the range filter on it where the column can be filtered by range.
   * The handles sit where the filter's bounds fall on the ramp under the ramp's scale, so a value's
   * handle sits on its colour.
   */
  private numeric(s: Store, column: Column, domain: {min: number; max: number}, colouring: Colouring): TemplateResult {
    const ramp = RAMPS[colouring.ramp];
    const stops = Array.from({length: 12}, (_, i) => rgb(colourOfFraction(i / 11, colouring.ramp, colouring.reverse))).join(', ');
    const fmt = (n: number) => {
      if (column.arrowType === 'timestamp_us') return dateText(n);
      if (Math.abs(n) >= 1e6 || (n !== 0 && Math.abs(n) < 1e-3)) return n.toExponential(2);
      return n.toLocaleString('en-GB', {maximumFractionDigits: 2});
    };
    const held = s.get('filters').draft.filter[column.name];
    const filterable = this.offers(column.name, 'numeric', 'range');
    const at = (v: number) => Math.min(1, Math.max(0, fractionOf(v, domain, colouring.scale, ramp.diverging)));
    const valueAt = (t: number) => valueAtFraction(t, domain, colouring.scale, ramp.diverging);
    // The filter's own bounds, `null` where an end is open. A bound outside the values drawn keeps
    // its value and pins its handle to the ramp's end.
    const bounds = held?.family === 'numeric' ? {gte: held.gte, lte: held.lte} : {gte: null, lte: null};
    const toBound = (t: number) => (t <= 0 || t >= 1 ? null : valueAt(t));
    const drag = this.dragging;
    const shown = drag ? {gte: toBound(drag.low), lte: toBound(drag.high)} : bounds;
    const low = drag ? drag.low : bounds.gte === null ? 0 : at(bounds.gte);
    const high = drag ? drag.high : bounds.lte === null ? 1 : at(bounds.lte);
    const range = drag !== null || bounds.gte !== null || bounds.lte !== null;
    const pct = (t: number) => `${(t * 100).toFixed(2)}%`;
    // A label under a handle slides from left-aligned at the ramp's start to right-aligned at its
    // end, so it stays on the card.
    const slide = (t: number) => `left:${pct(t)};transform:translateX(-${pct(t)})`;
    const caption = columnCaption(column.name);
    const commit = (gte: number | null, lte: number | null) => this.applyRange(column, gte, lte);
    const key = (end: 'low' | 'high') => (e: KeyboardEvent) => {
      const step = e.shiftKey ? 0.1 : 0.01;
      const from = end === 'low' ? low : high;
      const next = {ArrowLeft: from - step, ArrowDown: from - step, ArrowRight: from + step, ArrowUp: from + step, Home: 0, End: 1}[e.key];
      if (next === undefined) return;
      e.preventDefault();
      const t = Math.min(1, Math.max(0, next));
      // The end not moved keeps the filter's own bound.
      if (end === 'low') commit(t >= high ? bounds.lte : toBound(t), bounds.lte);
      else commit(bounds.gte, t <= low ? bounds.gte : toBound(t));
    };
    const aria = (bound: number | null, open: string) => ({now: bound ?? (open === 'lower' ? domain.min : domain.max), text: bound === null ? `No ${open} limit` : fmt(bound)});
    const lowAt = aria(shown.gte, 'lower');
    const highAt = aria(shown.lte, 'upper');
    const labels = html`${shown.gte !== null ? html`<span part="range-value" style=${slide(low)}>${fmt(shown.gte)}</span>` : nothing}${shown.lte !== null
      ? html`<span part="range-value" style=${slide(high)}>${fmt(shown.lte)}</span>`
      : nothing}`;
    const start = (e: PointerEvent) => this.dragStart(e, low, high, bounds, toBound, commit);
    return html`<div class=${`track${filterable ? ' filterable' : ''}`} @pointerdown=${filterable ? start : nothing}
        @pointermove=${filterable ? (e: PointerEvent) => this.dragMove(e) : nothing} @pointerup=${filterable ? (e: PointerEvent) => this.dragEnd(e) : nothing}
        @pointercancel=${() => this.dragCancel()}>
        <div part="ramp" style=${`background:linear-gradient(to right, ${stops})`}></div>
        ${range ? html`<div part="range" style=${`left:${pct(low)};right:${pct(1 - high)}`}></div>` : nothing}
        ${filterable
          ? html`<span part="range-low" role="slider" tabindex="0" style=${`left:${pct(low)}`} aria-label=${`Lowest ${caption}`} aria-valuemin=${domain.min} aria-valuemax=${domain.max}
                aria-valuenow=${lowAt.now} aria-valuetext=${lowAt.text} @keydown=${key('low')}></span
              ><span part="range-high" role="slider" tabindex="0" style=${`left:${pct(high)}`} aria-label=${`Highest ${caption}`} aria-valuemin=${domain.min} aria-valuemax=${domain.max}
                aria-valuenow=${highAt.now} aria-valuetext=${highAt.text} @keydown=${key('high')}></span>`
          : nothing}
      </div>
      <div class="axis">
        ${shown.gte === null || low > 0.18 ? html`<span part="value" class="lo">${fmt(domain.min)}</span>` : nothing}${range ? labels : nothing}${shown.lte === null || high < 0.82
          ? html`<span part="value" class="hi">${fmt(domain.max)}</span>`
          : nothing}
      </div>`;
  }

  /** Where a pointer is along the ramp, from 0 to 1. */
  private along(e: PointerEvent): number {
    const track = (e.currentTarget as HTMLElement).getBoundingClientRect();
    return track.width > 0 ? Math.min(1, Math.max(0, (e.clientX - track.left) / track.width)) : 0;
  }

  private dragStart(
    e: PointerEvent,
    low: number,
    high: number,
    bounds: {gte: number | null; lte: number | null},
    toBound: (t: number) => number | null,
    commit: (gte: number | null, lte: number | null) => void
  ): void {
    if (e.button !== 0) return;
    const t = this.along(e);
    const width = (e.currentTarget as HTMLElement).getBoundingClientRect().width || 1;
    // Near a handle, the drag moves it and the other end keeps the filter's own bound; elsewhere it
    // draws a new range from the pointer.
    const near = 8 / width;
    const end = Math.abs(t - low) <= near ? 'low' : Math.abs(t - high) <= near ? 'high' : 'new';
    const anchor = end === 'low' ? high : end === 'high' ? low : t;
    const keep = end === 'low' ? bounds.lte : end === 'high' ? bounds.gte : undefined;
    this.dragFrom = {anchor, keep, toBound, commit, start: t};
    this.dragging = end === 'new' ? {low: t, high: t} : {low, high};
    (e.currentTarget as HTMLElement).setPointerCapture?.(e.pointerId);
    e.preventDefault();
  }

  private dragMove(e: PointerEvent): void {
    if (!this.dragFrom) return;
    const t = this.along(e);
    const a = this.dragFrom.anchor;
    this.dragging = {low: Math.min(a, t), high: Math.max(a, t)};
  }

  private dragEnd(e: PointerEvent): void {
    const from = this.dragFrom;
    if (!from || !this.dragging) return;
    const t = this.along(e);
    this.dragFrom = null;
    this.dragging = null;
    // A press without a drag changes nothing.
    if (Math.abs(t - from.start) < 0.005) return;
    const moving = from.toBound(t);
    const fixed = from.keep !== undefined ? from.keep : from.toBound(from.anchor);
    if (t < from.anchor) from.commit(moving, fixed);
    else from.commit(fixed, moving);
  }

  private dragCancel(): void {
    this.dragFrom = null;
    this.dragging = null;
  }

  /** The Colour by menu: the choices, then the palettes for a category column or the ramps for a number. */
  private colourMenu(options: ColourOption[], column: Column | null, colouring: Colouring): TemplateResult {
    const at = Math.max(0, options.findIndex((o) => o.checked));
    const radio = <T,>(items: readonly T[], checked: (item: T) => boolean, pick: (item: T) => void, i: number, item: T) => ({
      role: 'radio',
      checked: checked(item) ? 'true' : 'false',
      tabindex: checked(item) || (i === 0 && !items.some(checked)) ? '0' : '-1',
      click: () => pick(item),
      keydown: (e: KeyboardEvent) => radioKeys(e, items.length, i, (j) => pick(items[j]!))
    });
    const names = Object.keys(CATEGORY_PALETTES) as CategoryPaletteName[];
    const ramps = Object.keys(RAMPS) as RampName[];
    const scales: RampScale[] = ['linear', 'log'];
    const palettes =
      !this.hidePalettes && column?.category
        ? html`<div class="hd rule" id="palette-label">Palette</div>
            <div role="radiogroup" aria-labelledby="palette-label">
              ${names.map((name, i) => {
                const r = radio(names, (n) => n === colouring.palette, (n) => this.choosePalette({palette: n}), i, name);
                const p = CATEGORY_PALETTES[name];
                return html`<button part="palette" type="button" role="radio" data-palette=${name} aria-checked=${r.checked} tabindex=${r.tabindex} @click=${r.click} @keydown=${r.keydown}>
                  <span class="pt">${p.title}${p.colourBlindSafe ? html`<span class="tag">Colour-blind safe</span>` : nothing}</span>
                  <span class="strip">${p.colours.slice(0, 8).map((c) => html`<span style=${`background:${hexOf(c)}`}></span>`)}</span>
                </button>`;
              })}
            </div>`
        : nothing;
    const rampChoices =
      !this.hidePalettes && column && !column.category
        ? html`<div class="hd rule" id="ramp-label">Ramp for ${columnCaption(column.name)}</div>
            <div role="radiogroup" aria-labelledby="ramp-label">
              ${ramps.map((name, i) => {
                const r = radio(ramps, (n) => n === colouring.ramp, (n) => this.choosePalette({ramp: n}), i, name);
                const bar = Array.from({length: 8}, (_, k) => rgb(colourOfFraction(k / 7, name, colouring.reverse))).join(', ');
                return html`<button part="ramp-option" type="button" role="radio" data-ramp=${name} aria-checked=${r.checked} tabindex=${r.tabindex} @click=${r.click} @keydown=${r.keydown}>
                  <span>${RAMPS[name].title}</span><span class="bar" style=${`background:linear-gradient(to right, ${bar})`}></span>
                </button>`;
              })}
            </div>
            <div class="opts">
              <span id="scale-label">Scale</span>
              <div part="scale" class="seg2" role="radiogroup" aria-labelledby="scale-label">
                ${scales.map((scale, i) => {
                  const r = radio(scales, (x) => x === colouring.scale, (x) => this.choosePalette({scale: x}), i, scale);
                  return html`<button type="button" role="radio" data-scale=${scale} aria-checked=${r.checked} tabindex=${r.tabindex} @click=${r.click} @keydown=${r.keydown}>${scale === 'linear' ? 'Linear' : 'Log'}</button>`;
                })}
              </div>
              <span id="reverse-label">Reverse</span>
              <button part="reverse" class="switch" type="button" role="switch" aria-checked=${colouring.reverse ? 'true' : 'false'} aria-labelledby="reverse-label"
                @click=${() => this.choosePalette({reverse: !colouring.reverse})}><span class="knob"></span></button>
            </div>`
        : nothing;
    return html`<div part="colour-menu" class="pop menu" popover="manual" role="dialog" aria-label="Colour by" @keydown=${this.onPopoverKey} @focusout=${this.onPopoverFocusOut}>
      <div class="hd" id="colour-by-label">Colour by</div>
      <div role="radiogroup" aria-labelledby="colour-by-label">
        ${options.map(
          (o, i) => html`<button part="option" type="button" role="radio" data-value=${o.value} data-kind=${o.cluster ? 'layer' : o.kind.toLowerCase() || 'none'} aria-checked=${o.checked ? 'true' : 'false'}
            tabindex=${i === at ? '0' : '-1'} @click=${() => this.choose(o.value)}
            @keydown=${(e: KeyboardEvent) => radioKeys(e, options.length, i, (j) => this.choose(options[j]!.value))}><span>${o.title}</span><span class="kind">${o.kind}</span></button>`
        )}
      </div>
      ${palettes}${rampChoices}
    </div>`;
  }

  /** Open the colour picker for one value, starting from its colour now. */
  private openPicker(picking: Picking, colour: string): void {
    this.menuOpen = false;
    const c = rgbOfHex(colour) ?? rgbOfHex(hexOf(parseRgb(colour))) ?? [0, 0, 0];
    this.hsv = hsvOf(c);
    this.picking = picking;
  }

  /** Give the value being picked `hex`, or its palette colour back where `hex` is null, and report it when `final`. */
  private pick(hex: string | null, final = true): void {
    const s = this.resolvedStore;
    const p = this.picking;
    if (!s || !p) return;
    setColouring(s, {values: withValueColour(colouringOf(s).values, p.column, p.key, hex)});
    if (final) emit(this, 'tessera-valuecolour', {column: p.column, value: p.key, colour: hex});
  }

  /** A colour waiting for the next frame, while a custom colour is dragged. */
  private livePick: {hex: string; frame: number} | null = null;

  /**
   * {@link pick} for the custom area and hue bar: a drag applies its colour at most once per
   * animation frame, and the final colour at once, so the map is not recoloured on every pointer
   * move.
   */
  private pickLive(hex: string, final: boolean): void {
    if (this.livePick) cancelAnimationFrame(this.livePick.frame);
    this.livePick = null;
    if (final || typeof requestAnimationFrame === 'undefined') {
      this.pick(hex, final);
      return;
    }
    this.livePick = {hex, frame: requestAnimationFrame(() => {
      this.livePick = null;
      this.pick(hex, false);
    })};
  }

  /** The colour picker for one value. */
  private colourPicker(p: Picking, colouring: Colouring, ranks: Record<number, number>, values: CategoryValue[]): TemplateResult {
    const palette = CATEGORY_PALETTES[colouring.palette].colours;
    const code = values.find((v) => v.key === p.key)?.code;
    const own = hexOf(colourOfRank(code === undefined ? undefined : ranks[code], colouring.palette));
    const current = colouring.values[p.column]?.[p.key] ?? own;
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
      return [r.width > 0 ? Math.min(359, Math.max(0, ((e.clientX - r.left) / r.width) * 360)) : h, sat, val];
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
    const svKey = (e: KeyboardEvent) => {
      const step = e.shiftKey ? 0.1 : 0.01;
      const clamp = (n: number) => Math.min(1, Math.max(0, n));
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
    return html`<div part="colour-popover" class="pop picker" popover="manual" role="dialog" aria-label=${`Colour of ${p.title}`} @keydown=${this.onPopoverKey} @focusout=${this.onPopoverFocusOut}>
      <div class="head"><span class="t">${p.title}</span><button part="reset" class="quiet" type="button" @click=${() => {
        this.hsv = hsvOf(rgbOfHex(own)!);
        this.pick(null);
      }}>Reset</button></div>
      <div class="choices" role="group" aria-label="Palette colours" style=${`grid-template-columns:repeat(${palette.length}, minmax(0, 1fr))`}>
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
          <span class="knob" style=${`left:${((h / 360) * 100).toFixed(1)}%`}></span>
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

  /** Escape closes an open popover and puts focus back on what opened it. */
  private onPopoverKey = (e: KeyboardEvent): void => {
    if (e.key !== 'Escape') return;
    e.stopPropagation();
    e.preventDefault();
    this.closePopovers(true);
  };

  /** Focus leaving an open popover for something outside it, as Tab past its last control does, closes it. */
  private onPopoverFocusOut = (e: FocusEvent): void => {
    const to = e.relatedTarget as Node | null;
    const pop = e.currentTarget as HTMLElement;
    if (to && !pop.contains(to)) this.closePopovers(false);
  };

  /** The frame loop that keeps an open popover beside what opened it as the page scrolls or resizes. */
  private following: {frame: number; at: string} | null = null;

  private follow(): void {
    if (this.following || typeof requestAnimationFrame === 'undefined') return;
    const tick = () => {
      const anchor = this.anchor();
      if (!anchor || !this.isConnected) {
        this.following = null;
        return;
      }
      const r = anchor.getBoundingClientRect();
      const at = `${r.left},${r.top},${innerWidth},${innerHeight}`;
      if (this.following && at !== this.following.at) this.requestUpdate();
      this.following = {frame: requestAnimationFrame(tick), at};
    };
    this.following = {frame: requestAnimationFrame(tick), at: ''};
  }

  /** A press outside an open popover and what opened it closes the popover. */
  private onOutside = (e: PointerEvent): void => {
    const path = e.composedPath();
    const inside = Array.from(this.renderRoot.querySelectorAll('.pop, [part="colour-by"], button[part="swatch"]')).some((el) => path.includes(el));
    if (!inside) this.closePopovers(false);
  };

  /** Close the menu and the picker; with `refocus`, focus goes back to the button that opened the one open. */
  private closePopovers(refocus: boolean): void {
    const anchor = refocus ? this.anchor() : null;
    this.menuOpen = false;
    this.picking = null;
    anchor?.focus();
  }

  /** The button the open popover belongs to. */
  private anchor(): HTMLElement | null {
    if (this.picking) return this.renderRoot.querySelector<HTMLElement>(`[part~="entry"][data-key="${CSS.escape(this.picking.key)}"] [part="swatch"]`);
    if (this.menuOpen) return this.renderRoot.querySelector<HTMLElement>('[part="colour-by"]');
    return null;
  }

  /**
   * An open popover is shown in the top layer, so no scrolling panel clips it, and placed beside
   * the button that opened it: the menu beneath the Colour by button, the picker to the right of
   * the legend, level with the swatch. Focus goes into it as it opens.
   */
  protected override updated(changed: PropertyValues<this>): void {
    const open = this.menuOpen || this.picking !== null;
    if (changed.has('menuOpen') || changed.has('picking')) {
      if (open) document.addEventListener('pointerdown', this.onOutside, true);
      else document.removeEventListener('pointerdown', this.onOutside, true);
    }
    const pop = this.renderRoot.querySelector<HTMLElement>('.pop');
    const anchor = this.anchor();
    if (!pop || !anchor) return;
    this.follow();
    const fresh = !pop.matches?.(':popover-open');
    if (fresh && typeof pop.showPopover === 'function') {
      try {
        pop.showPopover();
      } catch {
        // Shown already, or the popover API is absent; the element is in the page either way.
      }
    }
    const a = anchor.getBoundingClientRect();
    // The picker stands clear of the legend, beside the row it colours.
    const own = this.getBoundingClientRect();
    const width = pop.offsetWidth || 264;
    const height = pop.offsetHeight || 0;
    const vw = typeof innerWidth === 'number' ? innerWidth : 1024;
    const vh = typeof innerHeight === 'number' ? innerHeight : 768;
    const menu = pop.classList.contains('menu');
    let left = menu ? a.right - width : own.right + 12;
    let top = menu ? a.bottom + 6 : a.top - 40;
    if (!menu && left + width > vw - 8) left = own.left - width - 12;
    left = Math.max(8, Math.min(left, vw - width - 8));
    top = Math.max(8, Math.min(top, vh - height - 8));
    pop.style.left = `${Math.round(left)}px`;
    pop.style.top = `${Math.round(top)}px`;
    if (changed.has('menuOpen') || (changed.has('picking') && changed.get('picking') === null)) {
      const target = pop.querySelector<HTMLElement>('[aria-checked="true"], [aria-pressed="true"]') ?? pop.querySelector<HTMLElement>('button, [tabindex="0"]');
      target?.focus();
    }
  }
}

/** `rgb(r, g, b)` as RGB, for a swatch colour that is not a hex string. */
function parseRgb(text: string): [number, number, number] {
  const m = /rgba?\((\d+),\s*(\d+),\s*(\d+)/.exec(text);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : [0, 0, 0];
}

attachContextRoot();
defineOnce('tessera-legend', TesseraLegend);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-legend': TesseraLegend;
  }
}
