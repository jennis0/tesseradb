import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {CLUSTER_PREFIX, PALETTES, artifactColour, colourLayers, type AggregateSpec, type Layer, type Meta, type PaletteName, type Store} from '@mosaicajs/client';
import {CATEGORY_PALETTES} from '@mosaicajs/deck';
import {hexOf} from '@mosaicajs/deck/internal';
import {HeldAggregate, artifactGroupings, isTree, levelOf, listedGroups, rankedGrouping, type GroupCount} from './aggregate.js';
import {MosaicaElement, UNNAMED, columnCaption, countText, emit, idString, keyTitle} from './base.js';
import {ColourPicker, pickerStyles, type PickerTarget} from './colour-picker.js';
import {clusterColour, colouringOf, holdColours, paletteValueColour, setClusterColours, setValueColours, valueColour, valueMet, watchChoices} from './colouring.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {exportparts} from './parts.js';
import {ClusterPaths} from './paths.js';
import {chrome, tokens} from './tokens.js';
import './filter.js';
import './cluster-filter.js';

/** The height of a row in pixels, from which the rows in sight are worked out. */
const ROW_HEIGHT = 40;
/** The rows whose paths are asked for before the list has been measured. */
const FIRST_ROWS = 8;

/**
 * One row: a category value or a cluster, with its count over everything the viewer may see.
 * `unmet` marks a category value the map has given no colour yet.
 */
type Row = {key: string; name: string; path: string; count: number | null; colour: string; own: string; unmet: boolean; found: boolean};

/** A value or a cluster found with the search box, which the list shows above its own rows. */
type Found = {key: string; name: string; path: string; rung: number};

/**
 * Edit colours: every value of a category field, or every cluster of a layer, with its colour, to
 * search, choose colours for and reset. `field` is a column `meta.declaredScalars` lists as a
 * rendered category, or `cluster:<layer>` for a layer the points can be coloured by. `show()` opens
 * it as a modal dialog, and Close, Done or Escape closes it and gives focus back to what had it.
 *
 * The rows are a category's values, a flat layer's clusters, a `stacked` or `tiered` layer's
 * clusters at `cluster-level` (its deepest level where unset), or on a `nested` or `dag` layer the
 * clusters of the cut the map draws (`cut: 'drawn'`), which the layer's field card ranks too, and
 * which changes as the camera does. Each row has a checkbox, its colour, its name (a cluster's two
 * nearest parents in grey under it) and its count over everything the viewer may see, with no
 * filter and no area, so the rows hold their order, largest first, through a pan or a change of
 * filter. The counts come from one aggregate (`Store.setAggregate`, `subject: 'visible'`) of up to
 * `meta.selection.max_aggregate_top` rows, registered while the dialog is open. A category value
 * the map has not drawn yet has no palette colour, and its swatch is an outline until it has one.
 * Changing `field` or `cluster-level` while the dialog is open lists the new field's rows, and
 * closes the dialog where the field is neither a category nor a layer that colours.
 *
 * The search box is the field card's: `<mosaica-filter>`'s typeahead over a category's values, or
 * `<mosaica-cluster-filter>`'s over every name in the layer, at any depth of a tree. Choosing a
 * match that the list holds moves focus to its row; any other goes at the top of the list, with
 * its count, until the dialog closes.
 *
 * A row's colour is a button that opens the colour picker the field cards use. Ticking rows shows
 * how many are ticked, Set colour, which opens the picker and gives the colour chosen to every row
 * ticked, and Deselect. Reset to palette gives every value of the field, or every cluster of the
 * layer, its palette colour back. Under the list is how many colours are chosen for the field or
 * the layer.
 *
 * Each action fires one event: `mosaica-valuecolour` for a category, or `mosaica-clustercolour`
 * for a layer, naming every value or cluster it changed. A category's colours are written to the
 * colour choices every element over the store shares, and a layer's are set on the store with the
 * colours chosen for every layer (`Store.setArtifactColours`); the dialog keeps no colour itself.
 *
 * @summary Every value or cluster's colour, to search, choose and reset.
 * @tagname mosaica-colour-editor
 * @category Elements
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-valuecolour']>} mosaica-valuecolour - Colours
 *   were chosen or reset for values of a category.
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-clustercolour']>} mosaica-clustercolour - Colours
 *   were chosen or reset for clusters of a layer.
 * @csspart dialog - The dialog, while it is open.
 * @csspart title - The field's or the layer's name.
 * @csspart sub - Under the name: the level or the cut, and the palette.
 * @csspart close - The close button.
 * @csspart columns - The headings over the rows.
 * @csspart rows - The list of rows.
 * @csspart row - One row, with `data-key`, `data-found` where the search box found it, and
 *   `data-ticked` while it is ticked.
 * @csspart check - A row's checkbox.
 * @csspart swatch - A row's colour, a button that opens the colour picker, with `data-unmet` and
 *   drawn as an outline for a category value the map has given no colour yet.
 * @csspart name - A row's name, with `data-unnamed` for a cluster that has none.
 * @csspart path - A cluster row's parents.
 * @csspart count - A row's count over everything the viewer may see.
 * @csspart selected - The bar shown while rows are ticked.
 * @csspart set-colour - Its Set colour button.
 * @csspart deselect - Its Deselect button.
 * @csspart reset-all - The Reset to palette button.
 * @csspart changed - How many colours are chosen.
 * @csspart done - The Done button.
 * @csspart colour-popover - The colour picker, while it is open.
 * @csspart choice - A colour in the picker's palette rows, with `aria-pressed`.
 * @csspart sv - The picker's saturation and brightness area, a slider in two directions.
 * @csspart hue - The picker's hue bar, a slider.
 * @csspart hex - The picker's hex field.
 * @csspart reset - The picker's Reset button.
 * @csspart filter-<part> - A part of the inner `<mosaica-filter>`.
 * @csspart cluster-filter-<part> - A part of the inner `<mosaica-cluster-filter>`.
 */
export class MosaicaColourEditor extends MosaicaElement {
  static override styles = [
    tokens,
    chrome,
    pickerStyles,
    css`
      :host {
        display: contents;
      }
      [part='dialog'] {
        width: min(520px, calc(100vw - 32px));
        height: min(720px, calc(100dvh - 32px));
        max-width: none;
        max-height: none;
        padding: 0;
        border: 1px solid var(--_mosaica-line);
        border-radius: var(--_mosaica-radius);
        background: var(--_mosaica-surface);
        color: var(--_mosaica-ink);
        box-shadow: 0 16px 48px rgba(0, 0, 0, 0.22);
        font-family: var(--_mosaica-font);
        font-size: 13px;
        line-height: 1.45;
      }
      [part='dialog']::backdrop {
        background: rgba(27, 29, 33, 0.28);
      }
      [part='dialog']:focus {
        outline: none;
      }
      .box {
        height: 100%;
        display: flex;
        flex-direction: column;
      }
      .head {
        display: flex;
        align-items: center;
        gap: 10px;
        padding: 14px 16px 10px;
      }
      .names {
        flex: 1 1 auto;
        min-width: 0;
      }
      [part='title'] {
        display: block;
        margin: 0;
        font-size: 15px;
        font-weight: 600;
        letter-spacing: 0;
        text-transform: none;
        color: var(--_mosaica-ink);
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      [part='sub'] {
        font-size: 12px;
        color: var(--_mosaica-ink-2);
      }
      [part='close'] {
        flex: none;
        width: 28px;
        height: 28px;
        display: grid;
        place-items: center;
        border-radius: 6px;
        color: var(--_mosaica-ink-2);
      }
      [part='close']:hover {
        background: var(--_mosaica-surface-2);
      }
      .search {
        display: block;
        padding: 0 16px 10px;
        --_mosaica-input-height: 30px;
      }
      [part='selected'] {
        display: flex;
        align-items: center;
        gap: 10px;
        margin: 0 16px 4px;
        padding: 6px 10px;
        border-radius: 6px;
        background: var(--_mosaica-accent);
        color: var(--_mosaica-accent-ink);
        font-size: 12px;
      }
      [part='selected'] b {
        font-weight: 600;
        margin-right: auto;
      }
      [part='set-colour'] {
        display: inline-flex;
        align-items: center;
        gap: 6px;
        font-weight: 500;
      }
      [part='set-colour'] span {
        width: 18px;
        height: 18px;
        border: 2px solid currentColor;
        border-radius: 5px;
      }
      [part='deselect'] {
        text-decoration: underline;
        text-underline-offset: 2px;
      }
      [part='columns'],
      [part~='row'] {
        display: grid;
        grid-template-columns: 18px 26px minmax(0, 1fr) 72px;
        align-items: center;
        column-gap: 10px;
      }
      [part='columns'] {
        padding: 6px 16px 2px;
        font-size: 11px;
        color: var(--_mosaica-ink-3);
      }
      [part='columns'] span:last-child,
      [part='count'] {
        text-align: right;
      }
      [part='rows'] {
        flex: 1 1 auto;
        min-height: 0;
        overflow-y: auto;
        padding: 2px 8px 8px;
      }
      [part~='row'] {
        height: ${ROW_HEIGHT}px;
        padding: 0 8px;
        border-radius: 6px;
      }
      [part~='row']:hover,
      [part~='row'][data-ticked] {
        background: var(--_mosaica-surface-2);
      }
      [part~='row'][data-found] {
        box-shadow: inset 2px 0 0 var(--_mosaica-ink-3);
      }
      [part='check'] {
        width: 14px;
        height: 14px;
        margin: 0;
      }
      [part='swatch'] {
        width: 22px;
        height: 22px;
        border: 1px solid rgba(0, 0, 0, 0.12);
        border-radius: 5px;
        background: var(--c);
      }
      [part='swatch'][data-unmet] {
        background: none;
        border-color: var(--_mosaica-line-control);
      }
      .label {
        display: flex;
        flex-direction: column;
        min-width: 0;
      }
      [part='name'],
      [part='path'] {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      [part='path'] {
        font-size: 12px;
        color: var(--_mosaica-ink-3);
      }
      [part='count'] {
        font-size: 12px;
        color: var(--_mosaica-ink-2);
        font-variant-numeric: tabular-nums;
      }
      .none {
        padding: 12px 16px;
        font-size: 12px;
        color: var(--_mosaica-ink-3);
      }
      .foot {
        display: flex;
        align-items: center;
        gap: 10px;
        padding: 12px 16px;
        border-top: 1px solid var(--_mosaica-line-2);
      }
      [part='changed'] {
        flex: 1 1 auto;
        font-size: 12px;
        color: var(--_mosaica-ink-3);
      }
      @media (max-width: 480px) {
        [part='dialog'] {
          width: 100vw;
          height: 100dvh;
          border-radius: 0;
        }
      }
    `
  ];

  /** What the dialog colours: a rendered category column, or `cluster:<layer>`. */
  @property() accessor field = '';
  /** The level of a `stacked` or `tiered` layer whose clusters are listed; `null` is its deepest. */
  @property({type: Number, attribute: 'cluster-level'}) accessor level: number | null = null;

  /** Whether the dialog is open. @internal */
  @state() accessor isOpen = false;
  /** The ticked rows, by key. @internal */
  @state() accessor ticked: ReadonlySet<string> = new Set();
  /** What the search box found, newest first. @internal */
  @state() accessor found: readonly Found[] = [];

  private readonly ranked = new HeldAggregate('colours-ranked');
  private readonly foundCounts = new HeldAggregate('colours-found');
  private readonly paths = new ClusterPaths(() => this.requestUpdate());
  private readonly picker = new ColourPicker(this, () => this.renderRoot.querySelector('.box')?.getBoundingClientRect() ?? this.getBoundingClientRect());
  private unwatchChoices: (() => void) | null = null;
  /** What had focus as the dialog opened, which gets it back as it closes. */
  private opener: HTMLElement | null = null;
  /** The frame the paths of the rows in sight are next asked for in, while one is waiting. */
  private pathFrame: number | null = null;

  /** Open the dialog. */
  async show(): Promise<void> {
    this.opener = deepActive();
    this.isOpen = true;
    await this.updateComplete;
    // Nothing to edit: `field` names neither a category nor a layer that colours.
    if (!this.isOpen) return;
    const dialog = this.renderRoot.querySelector<HTMLDialogElement>('dialog');
    if (!dialog) return;
    if (!dialog.open) {
      if (typeof dialog.showModal === 'function') dialog.showModal();
      else dialog.setAttribute('open', '');
    }
    dialog.focus();
  }

  /** Close the dialog, giving focus back to what had it as it opened. */
  close(): void {
    if (!this.isOpen) return;
    this.picker.close(false);
    const dialog = this.renderRoot.querySelector<HTMLDialogElement>('dialog');
    if (dialog?.open) {
      if (typeof dialog.close === 'function') dialog.close();
      else dialog.removeAttribute('open');
    }
    this.isOpen = false;
    this.ticked = new Set();
    this.found = [];
    this.stopPaths();
    const opener = this.opener;
    this.opener = null;
    opener?.focus();
  }

  protected override onStoreAdopted(store: Store | null): void {
    this.unwatchChoices?.();
    this.unwatchChoices = store ? watchChoices(store, () => this.requestUpdate()) : null;
  }

  protected override resetServerData(): void {
    this.paths.reset();
    this.found = [];
    this.ticked = new Set();
    this.picker.close(false);
  }

  override disconnectedCallback(): void {
    this.ranked.set(null, null);
    this.foundCounts.set(null, null);
    this.stopPaths();
    this.unwatchChoices?.();
    this.unwatchChoices = null;
    super.disconnectedCallback();
  }

  /**
   * A change of field or level drops what was found and ticked and closes the picker, since each
   * names the last field's values; the dialog closes where the field is now nothing it can edit.
   */
  protected override willUpdate(changed: PropertyValues<this>): void {
    super.willUpdate(changed);
    if (changed.has('field') || changed.has('level')) {
      this.found = [];
      this.ticked = new Set();
      this.picker.close(false);
    }
    const meta = this.resolvedStore?.get('meta') ?? null;
    if (this.isOpen && (meta === null || !this.editable(meta))) this.close();
  }

  private get layerName(): string | null {
    return this.field.startsWith(CLUSTER_PREFIX) ? this.field.slice(CLUSTER_PREFIX.length) : null;
  }

  /** The layer listed, where `field` names one the points can be coloured by. */
  private layerOf(meta: Meta): Layer | null {
    const name = this.layerName;
    return name === null ? null : (colourLayers(meta.layers).find((l) => l.name === name) ?? null);
  }

  /** Whether `field` is a rendered category column. */
  private isCategory(meta: Meta): boolean {
    return this.layerName === null && meta.declaredScalars.some((c) => c.name === this.field && c.render && c.category);
  }

  private editable(meta: Meta): boolean {
    return this.isCategory(meta) || this.layerOf(meta) !== null;
  }

  /** The aggregate that ranks the rows, over everything the viewer may see. */
  private rankedSpec(meta: Meta): AggregateSpec | null {
    const top = meta.selection.maxAggregateTop;
    if (this.isCategory(meta)) return {groupings: [{by: {field: this.field, top}}], subject: 'visible'};
    const layer = this.layerOf(meta);
    return layer ? {groupings: [rankedGrouping(layer, top, this.level)], subject: 'visible'} : null;
  }

  /** The aggregate that counts what the search box found. */
  private foundSpec(meta: Meta): AggregateSpec | null {
    if (this.found.length === 0) return null;
    if (this.isCategory(meta)) return {groupings: [{by: {field: this.field, values: this.found.map((f) => f.key).slice(0, meta.selection.maxAggregateNamed).sort()}}], subject: 'visible'};
    const layer = this.layerOf(meta);
    if (!layer) return null;
    return {groupings: artifactGroupings(layer, this.found.map((f) => ({tesseraId: BigInt(f.key), rung: f.rung})), meta.selection, 'drawn'), subject: 'visible'};
  }

  protected override updated(changed: PropertyValues<this>): void {
    super.updated(changed);
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    const live = this.isOpen && this.isConnected && s !== null && meta !== null;
    this.ranked.set(s, live ? this.rankedSpec(meta) : null);
    this.foundCounts.set(s, live ? this.foundSpec(meta) : null);
    if (live && this.pathFrame === null) this.askPaths();
  }

  /** Ask for the paths of the rows in sight in the next frame, once however often the list scrolls. */
  private onScroll = (): void => {
    if (this.pathFrame !== null || typeof requestAnimationFrame === 'undefined') return;
    this.pathFrame = requestAnimationFrame(() => {
      this.pathFrame = null;
      this.askPaths();
    });
  };

  private stopPaths(): void {
    if (this.pathFrame !== null) cancelAnimationFrame(this.pathFrame);
    this.pathFrame = null;
  }

  /** Ask for the paths of the clusters in sight; before the list is measured, of its first few rows. */
  private askPaths(): void {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    const layer = meta ? this.layerOf(meta) : null;
    if (!s || !meta || !layer || !isTree(layer)) return;
    const list = this.renderRoot.querySelector<HTMLElement>('[part="rows"]');
    const height = list?.clientHeight ?? 0;
    const first = height > 0 ? Math.floor(list!.scrollTop / ROW_HEIGHT) : 0;
    const last = height > 0 ? Math.ceil((list!.scrollTop + height) / ROW_HEIGHT) : FIRST_ROWS;
    this.paths.ask(s, layer, this.rows(s, meta).slice(first, last).filter((r) => !r.found).map((r) => BigInt(r.key)));
  }

  /**
   * The rows: what the search box found, then the field's values or the layer's clusters, largest
   * first. Only answers to what the current field asks are read, so none is listed while the
   * answer for a field just chosen is on its way.
   */
  private rows(s: Store, meta: Meta): Row[] {
    const ranked = this.ranked.entryOf(s, this.rankedSpec(meta));
    const found = this.foundCounts.entryOf(s, this.foundSpec(meta));
    const counted = new Map<string, GroupCount>();
    for (const t of found?.result?.tables ?? []) for (const g of listedGroups(t)) counted.set(g.key, g);
    const listed = listedGroups(ranked?.result?.tables[0]);
    const inList = new Set(listed.map((g) => g.key));
    if (this.isCategory(meta)) {
      const chosen = colouringOf(s).values[this.field] ?? {};
      const row = (key: string, title: string | null, count: number | null, isFound: boolean, name?: string): Row => ({
        key,
        name: title ?? name ?? keyTitle(s, this.field, key),
        path: '',
        count,
        colour: valueColour(s, this.field, key),
        own: paletteValueColour(s, this.field, key),
        unmet: chosen[key] === undefined && !valueMet(s, this.field, key),
        found: isFound
      });
      return [
        ...this.found.filter((f) => !inList.has(f.key)).map((f) => row(f.key, counted.get(f.key)?.title ?? null, counted.get(f.key)?.count ?? null, true, f.name)),
        ...listed.map((g) => row(g.key, g.title, g.count, false))
      ];
    }
    const layer = this.layerName!;
    const palette = s.get('artifacts').palette;
    const cluster = (g: GroupCount | undefined, key: string, name: string, path: string, at: PaletteName, isFound: boolean): Row => {
      const own = hexOf(artifactColour(at, g?.slot ?? null));
      return {key, name, path, count: g?.count ?? null, colour: clusterColour(s, layer, BigInt(key), own), own, unmet: false, found: isFound};
    };
    return [
      ...this.found.filter((f) => !inList.has(f.key)).map((f) => cluster(counted.get(f.key), f.key, f.name, f.path, found?.palette ?? palette, true)),
      ...listed.map((g) => cluster(g, g.key, g.title ?? UNNAMED, this.paths.pathOf(BigInt(g.key)), ranked?.palette ?? palette, false))
    ];
  }

  /** The keys of the colours chosen for this field's values or this layer's clusters. */
  private chosenKeys(s: Store): string[] {
    const layer = this.layerName;
    if (layer === null) return Object.keys(colouringOf(s).values[this.field] ?? {});
    return [...(s.get('artifacts').overrides.get(layer)?.keys() ?? [])].map(idString);
  }

  /** Give the values or clusters `keys` the colour `hex`, or their palette colours back, as one action. */
  private apply(keys: readonly string[], hex: string | null, final: boolean): void {
    const s = this.resolvedStore;
    if (!s || keys.length === 0) return;
    const layer = this.layerName;
    if (layer === null) {
      const changes = keys.map((value) => ({value, colour: hex}));
      setValueColours(s, this.field, changes);
      if (final) emit(this, 'mosaica-valuecolour', {column: this.field, changes});
      return;
    }
    setClusterColours(s, layer, keys.map((k) => ({tesseraId: BigInt(k), colour: hex})));
    if (final) emit(this, 'mosaica-clustercolour', {layer, changes: keys.map((tesseraId) => ({tesseraId, colour: hex}))});
  }

  /** The palette the picker offers: the category palette, or the layer's. */
  private palette(s: Store): PickerTarget['palette'] {
    return this.layerName === null ? CATEGORY_PALETTES[colouringOf(s).palette] : PALETTES[s.get('artifacts').palette];
  }

  private pick(e: Event, title: string, rows: readonly Row[]): void {
    const s = this.resolvedStore;
    const first = rows[0];
    const layer = this.layerName;
    if (!s || !first) return;
    const keys = rows.map((r) => r.key);
    this.picker.open(e.currentTarget as HTMLElement, {
      title,
      palette: this.palette(s),
      own: first.own,
      current: () => (layer === null ? valueColour(s, this.field, first.key) : clusterColour(s, layer, BigInt(first.key), first.own)),
      apply: (hex, final) => this.apply(keys, hex, final),
      hold: () => holdColours(s)
    });
  }

  private tick(key: string, on: boolean): void {
    const next = new Set(this.ticked);
    if (on) next.add(key);
    else next.delete(key);
    this.ticked = next;
  }

  /** A match the search box found: its row gets focus where the list holds it; else it goes at the top. */
  private onFound(f: Found, listed: readonly Row[]): void {
    const at = listed.find((r) => r.key === f.key);
    if (!at || at.found) {
      this.found = [f, ...this.found.filter((x) => x.key !== f.key)];
      void this.updateComplete.then(() => this.renderRoot.querySelector('[part="rows"]')?.scrollTo?.({top: 0}));
    }
    void this.updateComplete.then(() => {
      const row = Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="row"]')).find((el) => el.dataset.key === f.key);
      row?.scrollIntoView?.({block: 'nearest'});
      row?.querySelector<HTMLElement>('[part="check"]')?.focus();
    });
  }

  private onKey = (e: KeyboardEvent): void => {
    if (e.key !== 'Escape') return;
    e.preventDefault();
    e.stopPropagation();
    this.close();
  };

  override render(): TemplateResult {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    const layer = meta ? this.layerOf(meta) : null;
    const name = layer ? layer.title || layer.name : columnCaption(this.field);
    // A dialog closed by the browser, as on a second Escape, is closed here too.
    const dialog = (body: TemplateResult | typeof nothing) =>
      html`<dialog part="dialog" aria-labelledby="title" tabindex="-1" @keydown=${this.onKey} @cancel=${(e: Event) => e.preventDefault()} @close=${() => this.close()}>${body}</dialog>`;
    if (!this.isOpen || !s || !meta || !this.editable(meta)) return dialog(nothing);
    const rows = this.rows(s, meta);
    const ranked = this.ranked.entryOf(s, this.rankedSpec(meta));
    const table = ranked?.result?.tables[0];
    const level = layer ? levelOf(layer, this.level) : undefined;
    const levelTitle = layer && level !== undefined && layer.levels.length > 1 ? (layer.levels.find((l) => l.level === level)?.title ?? `Level ${level}`) : null;
    const where = layer && isTree(layer) ? 'Clusters drawn' : levelTitle;
    const sub = [where, this.palette(s).title].filter(Boolean).join(' · ');
    const noun = layer ? 'clusters' : 'values';
    // A layer's search reaches every cluster, more than the rows listed; a category's, its values.
    const placeholder = layer || table?.groups == null ? `Search ${noun}` : `Search ${table.groups.toLocaleString('en-GB')} ${noun}`;
    const choose = (f: Found) => this.onFound(f, rows);
    const search = layer
      ? html`<mosaica-cluster-filter class="search" exportparts=${exportparts('cluster-filter')} layer=${layer.name} placeholder=${placeholder} .store=${s}
          .choose=${(c: {id: string; name: string | null; path: string; rung: number}) => choose({key: c.id, name: c.name ?? UNNAMED, path: c.path, rung: c.rung})}></mosaica-cluster-filter>`
      : html`<mosaica-filter class="search" exportparts=${exportparts('filter')} column=${this.field} placeholder=${placeholder} .store=${s}
          .choose=${(v: {key: string; title: string | null}) => choose({key: v.key, name: v.title ?? keyTitle(s, this.field, v.key), path: '', rung: 0})}></mosaica-filter>`;
    const ticked = rows.filter((r) => this.ticked.has(r.key));
    const bar =
      ticked.length > 0
        ? html`<div part="selected" role="group" aria-label="Ticked rows">
            <b>${ticked.length.toLocaleString('en-GB')} selected</b>
            <button part="set-colour" type="button" aria-haspopup="dialog" @click=${(e: Event) => this.pick(e, `${ticked.length.toLocaleString('en-GB')} selected`, ticked)}>Set colour<span style=${`background:${ticked[0]!.colour}`}></span></button>
            <button part="deselect" type="button" @click=${() => (this.ticked = new Set())}>Deselect</button>
          </div>`
        : nothing;
    const row = (r: Row) => {
      const on = this.ticked.has(r.key);
      return html`<div part="row" role="listitem" data-key=${r.key} ?data-found=${r.found} ?data-ticked=${on}>
        <input part="check" type="checkbox" .checked=${on} aria-label=${`Select ${r.name}`} @change=${(e: Event) => this.tick(r.key, (e.target as HTMLInputElement).checked)} />
        <button part="swatch" type="button" style=${`--c:${r.colour}`} ?data-unmet=${r.unmet} aria-haspopup="dialog" aria-label=${`Colour of ${r.name}`} title=${r.unmet ? 'Not drawn yet' : r.colour} @click=${(e: Event) => this.pick(e, r.name, [r])}></button>
        <span class="label"><span part="name" title=${r.name} ?data-unnamed=${r.name === UNNAMED}>${r.name}</span>${r.path ? html`<span part="path" title=${r.path}>${r.path}</span>` : nothing}</span>
        <span part="count">${r.count === null ? '' : countText(r.count)}</span>
      </div>`;
    };
    const empty = rows.length === 0 ? html`<div class="none">${ranked?.status === 'refused' ? `${layer ? 'Clusters' : 'Values'} unavailable` : ranked?.result ? 'Nothing to colour' : 'Loading…'}</div>` : nothing;
    const chosen = this.chosenKeys(s);
    return dialog(html`<div class="box">
      <div class="head">
        <div class="names"><h2 part="title" id="title">${name}</h2><div part="sub">${sub}</div></div>
        <button part="close" type="button" aria-label="Close" @click=${() => this.close()}>${icon('close', 14, 2)}</button>
      </div>
      ${search}${bar}
      <div part="columns" aria-hidden="true"><span></span><span></span><span>${layer ? 'Cluster' : 'Value'}</span><span>Items</span></div>
      <div part="rows" role="list" aria-label=${`${name}: ${noun} by size`} @scroll=${this.onScroll}>${repeat(rows, (r) => r.key, row)}${empty}</div>
      <div class="foot">
        <button part="reset-all" class="btn" type="button" ?disabled=${chosen.length === 0} @click=${() => this.apply(chosen, null, true)}>Reset to palette</button>
        <span part="changed" role="status">${chosen.length === 1 ? '1 colour changed' : `${chosen.length.toLocaleString('en-GB')} colours changed`}</span>
        <button part="done" class="btn primary" type="button" @click=${() => this.close()}>Done</button>
      </div>
      ${this.picker.render()}
    </div>`);
  }
}

/** The element focus is in, through every open shadow root. */
function deepActive(): HTMLElement | null {
  let at: Element | null = document.activeElement;
  while (at?.shadowRoot?.activeElement) at = at.shadowRoot.activeElement;
  return at instanceof HTMLElement && at !== document.body ? at : null;
}

attachContextRoot();
defineOnce('mosaica-colour-editor', MosaicaColourEditor);

declare global {
  interface HTMLElementTagNameMap {
    'mosaica-colour-editor': MosaicaColourEditor;
  }
}
