import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {CLUSTER_PREFIX, activeCount, artifactName, emptyDraft, isPopulated, withoutClause, withoutMember, type ClauseVerb, type ColumnDraft, type Layer, type MemberClause, type Meta} from '@tesseradb/client';
import {OPERATOR_WORDS, type TesseraFilter} from './filter.js';
import {TesseraElement, UNNAMED, columnCaption, dateRangeText, emit, keyTitle} from './base.js';
import {radioKeys} from './display.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {exportparts} from './parts.js';
import {FloatingList} from './float.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';
import './filter.js';
import './cluster-filter.js';

/** Each control's parts, forwarded as `filter-<part>` and `cluster-filter-<part>` (`parts.ts`). */
const FILTER_PARTS = exportparts('filter');
const CLUSTER_PARTS = exportparts('cluster-filter');

/** The layers whose clusters a viewer can filter by in `view`: those `meta` lists for it that attach to no other. */
export function clusterFieldLayers(meta: Meta, view: string): Layer[] {
  return meta.layers.filter((l) => l.depsOn.length === 0 && (view === '' || l.views.includes(view)));
}

/** A layer's field as the panel keys it, apart from any column's. */
const layerField = (layer: string) => `${CLUSTER_PREFIX}${layer}`;

const MODES: [ClauseVerb, string][] = [
  ['filter', 'Filter'],
  ['highlight', 'Highlight']
];

/**
 * The filter controls under a Filters heading with Clear all, and a Filter / Highlight switch that
 * counts the clauses in each position. A clause is in one of two positions: a filter narrows the
 * map and every count to the matches, and a highlight lights the matches among what the filter
 * keeps. A column or an artifact can hold a clause in each. The two are independent: neither
 * changes the other, and each has its own chip under `chips-only`. A highlight chip carries the
 * highlight mark and colour; a filter chip carries none. A `member_of` clause (a cluster chosen in
 * a cluster field, on the artifact card or in the hierarchy) is a chip too.
 *
 * The fields are the filterable columns and the layers whose clusters can be filtered by: every
 * layer `meta` lists for the current view that attaches to no other. A layer's field is a
 * `<tessera-cluster-filter>`, whose clusters join as `member_of` clauses. The switch sets `mode`,
 * the position every field edits. The fields listed are the columns in `pinned`, those holding a
 * clause in either position, in `meta`'s order, then the layers holding a clause, and any field the
 * user has opened or changed. A listed field shows its control while it holds a clause in the
 * current position or the user opened or changed it, and otherwise a row reading "Any" that opens
 * it. Add filter lists every field, the columns then the layers, with a search box, and the listed
 * ones are checked; its list opens over what sits below it. Choosing an unchecked field lists and
 * opens it. Choosing a checked one takes it off the panel and empties its clauses in both
 * positions; a column in `pinned` stays, since the host lists it. Enter in the search box adds the
 * first match not yet listed and never takes one off.
 *
 * Pressing a column's chip sets `mode` to the chip's position and opens, scrolls to and focuses the
 * column's control; under `chips-only`, where there are no controls, it fires `tessera-chipopen`
 * instead, for a host to show them with {@link TesseraFilterPanel.show}. Removing a chip empties
 * its clause and leaves the other position's alone. Clear all empties every control in both
 * positions and drops every `member_of` clause. `chips-only` renders the heading and the chips
 * without the controls, and nothing while no clause is applied; `controls-only` leaves the heading
 * out.
 *
 * A `member_of` clause on a layer with no field here, one that attaches to another layer or one of
 * another view, is still applied, so it is listed under "Also applied" as a chip that takes it off.
 *
 * @summary The filter controls, and the applied clauses as chips.
 * @tagname tessera-filter-panel
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A
 *   column's chip was removed (with `verb`, the position it was in, and `expr` null), or Clear all
 *   was pressed (with `column` and `expr` null). Removing a `member_of` chip fires nothing. Each
 *   inner control fires its own as well.
 * @fires {CustomEvent<TesseraEventDetails['tessera-chipopen']>} tessera-chipopen - A column's chip
 *   was pressed under `chips-only`, naming the column and the position its control is to edit.
 * @csspart title - The Filters heading, holding Clear all at its right.
 * @csspart clear - The Clear all button, shown while any clause is applied.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - The words "View refused", with `data-code`, in the refused state.
 * @csspart chips - The applied clauses.
 * @csspart chip - One applied clause, with `data-verb` (`filter` or `highlight`) and `data-column`
 *   or `data-artifact`.
 * @csspart edit - The button that is a column chip's text, which opens its control.
 * @csspart verb - The highlight mark on a highlight chip.
 * @csspart mode - The Filter / Highlight switch: two radio buttons with `data-verb` and
 *   `aria-checked`, each holding its count.
 * @csspart mode-count - The number of clauses in a position, in its button.
 * @csspart field - A listed field's section, with `data-column` for a column or `data-layer` for a
 *   layer, and `data-open` while its control shows.
 * @csspart any - A closed column's row, the button that opens its control.
 * @csspart others - The clauses on layers with no field here, such as a layer that attaches to
 *   another or one of another view, as chips that take them off.
 * @csspart add - The Add filter button, with `aria-expanded`.
 * @csspart add-list - The list of columns to add, while it is open.
 * @csspart add-search - The search box over that list.
 * @csspart add-option - One field in that list, with `data-column` or `data-layer`, `aria-checked`
 *   while it is listed, and `aria-disabled` where it is pinned and so cannot be taken off.
 * @csspart filter-<part> - A part of an inner `<tessera-filter>`, forwarded under a `filter-`
 *   prefix: `filter-entry`, `filter-tick`, and so on.
 * @csspart cluster-filter-<part> - A part of an inner `<tessera-cluster-filter>`, forwarded under a
 *   `cluster-filter-` prefix.
 */
export class TesseraFilterPanel extends TesseraElement {
  /** Renders the heading and the chips without the controls, and nothing while no clause is applied. */
  @property({type: Boolean, attribute: 'chips-only'}) accessor chipsOnly = false;
  /** Renders the switch and the controls without the chips. */
  @property({type: Boolean, attribute: 'controls-only'}) accessor controlsOnly = false;
  /** The position every control edits: the `filter` clauses or the `highlight` clauses. */
  @property({reflect: true}) accessor mode: ClauseVerb = 'filter';
  /**
   * The columns whose controls are listed whether or not they hold a clause, space- or
   * comma-separated. Unset, only the columns holding a clause are listed until the user adds one.
   */
  @property() accessor pinned = '';

  /** The columns the user opened, which stay listed and open. @internal */
  @state() accessor opened: ReadonlySet<string> = new Set();
  /** Whether the Add filter list is open, and its search. @internal */
  @state() accessor adding = false;
  /** @internal */
  @state() accessor addSearch = '';
  /** The Add filter option in the tab order, the one the arrow keys reached last. @internal */
  @state() accessor addActive = 0;

  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='chips'] {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
      }
      .chip .edit {
        display: inline-flex;
        flex: 0 1 auto;
        align-items: center;
        gap: 6px;
        color: inherit;
        font: inherit;
        text-align: left;
      }
      .chip .mark {
        display: inline-flex;
        flex: none;
      }
      .head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 10px;
        padding: 10px var(--_tessera-panel-inline, 16px);
        border-bottom: 1px solid var(--_tessera-line-2);
      }
      .head [part='title'] {
        margin: 0;
        flex: 1;
      }
      .head.top {
        padding-bottom: 8px;
        border-bottom: 0;
      }
      .head.top ~ .mode-row {
        padding-top: 0;
      }
      [part='mode'] {
        flex: 1;
        display: flex;
        padding: 2px;
        gap: 2px;
        background: var(--_tessera-surface-3);
        border-radius: 7px;
      }
      [part='mode'] button {
        flex: 1 1 0;
        display: flex;
        align-items: center;
        justify-content: center;
        gap: 6px;
        height: 26px;
        padding: 0 10px;
        border-radius: 5px;
        font-size: 12px;
        font-weight: 500;
        color: var(--_tessera-ink-2);
      }
      [part='mode'] button[aria-checked='true'] {
        background: var(--_tessera-surface);
        box-shadow: 0 1px 2px rgba(0, 0, 0, 0.08);
        font-weight: 600;
        color: var(--_tessera-ink);
      }
      [part='mode'] button[data-verb='highlight'][aria-checked='true'] {
        color: var(--_tessera-highlight);
      }
      [part='mode-count'] {
        font-weight: 500;
        color: var(--_tessera-ink-3);
        font-variant-numeric: tabular-nums;
      }
      [aria-checked='true'] [part='mode-count'] {
        color: var(--_tessera-ink-2);
      }
      [part~='field'] {
        padding: 12px var(--_tessera-panel-inline, 16px) 14px;
        border-bottom: 1px solid var(--_tessera-line-2);
      }
      [part~='field']:not([data-open]) {
        padding: 0;
      }
      [part='any'] {
        display: flex;
        align-items: baseline;
        justify-content: space-between;
        width: 100%;
        padding: 12px var(--_tessera-panel-inline, 16px);
        text-align: left;
      }
      [part='any']:hover {
        background: var(--_tessera-surface-2);
      }
      [part='any'] .n {
        font-weight: 600;
      }
      [part='any'] .v {
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
      .others {
        display: flex;
        flex-direction: column;
        gap: 8px;
        padding: 12px var(--_tessera-panel-inline, 16px) 14px;
        border-bottom: 1px solid var(--_tessera-line-2);
      }
      .others .n {
        font-weight: 600;
      }
      .adder {
        position: relative;
        padding: 10px var(--_tessera-panel-inline, 16px) 12px;
      }
      [part='add'] {
        height: auto;
        padding: 5px 10px;
      }
      /* The list opens over what sits below it, in the top layer. */
      [part='add-list'] {
        position: fixed;
        inset: auto;
        margin: 0;
        box-sizing: border-box;
        overflow-y: auto;
        background: var(--_tessera-surface);
        color: var(--_tessera-ink);
        font-size: 13px;
        padding: 4px;
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius-control);
        box-shadow: 0 6px 18px rgba(0, 0, 0, 0.08);
      }
      [part='add-list'] .input {
        margin-bottom: 4px;
      }
      [part~='add-option'] {
        display: flex;
        justify-content: space-between;
        gap: 8px;
        width: 100%;
        padding: 6px 8px;
        border-radius: 4px;
        text-align: left;
      }
      [part~='add-option']:hover,
      [part~='add-option']:focus-visible,
      [part~='add-option'][aria-checked='true'] {
        background: var(--_tessera-surface-2);
      }
      [part~='add-option'][aria-checked='true'] {
        font-weight: 500;
      }
      [part~='add-option']:focus-visible {
        outline-offset: -2px;
      }
      [part~='add-option'][aria-disabled='true'] {
        cursor: default;
      }
      .adder .anchor {
        height: 0;
        margin-bottom: 0;
      }
      [part~='add-option'] .kind {
        font-size: 12px;
        font-weight: 400;
        color: var(--_tessera-ink-3);
      }
      .none {
        display: block;
        padding: 6px 8px;
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
    `
  ];

  private chipText(column: string, draft: ColumnDraft): string {
    const caption = columnCaption(column);
    switch (draft.family) {
      case 'text':
        return `${caption}: ${draft.expr !== undefined ? JSON.stringify(draft.expr) : draft.query.trim().replace(/"([^"]*)"?/g, '“$1”')}`;
      case 'keyword':
        return `${caption} ${OPERATOR_WORDS[draft.op]} ${draft.needle}`;
      case 'category':
        return `${caption}: ${draft.keys.map((k) => keyTitle(this.resolvedStore, column, k)).join(', ')}`;
      case 'numeric': {
        const meta = this.resolvedStore?.get('meta');
        if (meta?.declaredScalars.find((c) => c.name === column)?.arrowType === 'timestamp_us') return `${caption}: ${dateRangeText(draft.gte, draft.lte)}`;
        const f = (v: number) => v.toLocaleString('en-GB');
        if (draft.lte === null) return `${caption} ≥ ${f(draft.gte!)}`;
        if (draft.gte === null) return `${caption} ≤ ${f(draft.lte)}`;
        return `${caption} ${f(draft.gte)}–${f(draft.lte)}`;
      }
    }
  }

  /** The mark a highlight chip carries. */
  private mark(verb: ClauseVerb) {
    return verb === 'highlight' ? html`<span class="mark" part="verb" title="Highlight">${icon('highlight', 12)}</span>` : nothing;
  }

  /** The control to open, scroll to and focus once the controls are drawn. */
  private editing: string | null = null;
  private readonly floating = new FloatingList(() => {
    const list = this.renderRoot.querySelector<HTMLElement>('[part="add-list"]');
    const anchor = this.renderRoot.querySelector<HTMLElement>('.adder .anchor');
    return list && anchor ? {list, anchor} : null;
  });

  override disconnectedCallback(): void {
    this.floating.stop();
    super.disconnectedCallback();
  }

  /**
   * Set `mode` to `verb`, open `column`'s control, and scroll to and focus it once drawn. A host
   * showing the controls after `tessera-chipopen` calls this.
   */
  show(column: string, verb: ClauseVerb): void {
    this.mode = verb;
    this.opened = new Set([...this.opened, column]);
    this.editing = column;
    this.requestUpdate();
  }

  /** Moves focus to the Filter / Highlight switch, where the controls are drawn. */
  override focus(options?: FocusOptions): void {
    const target = this.renderRoot.querySelector<HTMLElement>('[part="mode"] [aria-checked="true"]');
    if (target) target.focus(options);
    else super.focus(options);
  }

  private edit(column: string, verb: ClauseVerb): void {
    if (this.chipsOnly) emit(this, 'tessera-chipopen', {column, verb});
    else this.show(column, verb);
  }

  protected override updated(changed: PropertyValues<this>): void {
    super.updated(changed);
    this.floating.update();
    if (!this.editing || this.chipsOnly) return;
    const control = Array.from(this.renderRoot.querySelectorAll<HTMLElement & {updateComplete: Promise<unknown>}>('[part~="field"]'))
      .find((f) => (f.dataset.column ?? layerField(f.dataset.layer ?? '')) === this.editing)
      ?.querySelector<TesseraFilter>('tessera-filter, tessera-cluster-filter');
    if (!control) return;
    this.editing = null;
    void control.updateComplete.then(() => {
      control.scrollIntoView({block: 'nearest'});
      control.focus();
    });
  }

  private clearMember(clause: MemberClause): void {
    const s = this.resolvedStore;
    if (!s) return;
    s.setMembers(withoutMember(s.get('filters').members, clause.layer, clause.artifact, clause.verb));
  }

  /**
   * A `member_of` chip's text: the clause's own label (the only name a filter layer's artifact has,
   * since it is not served), else the served name, else unnamed.
   */
  private memberText(clause: MemberClause): string {
    const artifacts = this.resolvedStore?.get('artifacts');
    const served = artifacts?.served.find((a) => a.tesseraId === clause.artifact && a.layer === clause.layer);
    const name = clause.label ?? (served ? artifactName(served, artifacts!.attached) : null) ?? UNNAMED;
    return clause.outside ? `Outside ${name}` : name;
  }

  private clearColumn(column: string, verb: ClauseVerb): void {
    const s = this.resolvedStore;
    if (!s) return;
    s.setFilters(withoutClause(s.get('filters').draft, column, verb));
    emit(this, 'tessera-filterchange', {column, verb, expr: null});
  }

  private clearAll(): void {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    if (!s || !meta) return;
    s.setFilters(emptyDraft(meta.filterOperands));
    s.setMembers([]);
    emit(this, 'tessera-filterchange', {column: null, expr: null});
  }

  private chooseMode(verb: ClauseVerb): void {
    this.mode = verb;
  }

  private keepOpen(column: string): void {
    if (!this.opened.has(column)) this.opened = new Set([...this.opened, column]);
  }

  private add(column: string): void {
    this.adding = false;
    this.addSearch = '';
    this.show(column, this.mode);
  }

  /** Take a field off the panel, emptying its clauses in both positions. */
  private takeOff(column: string): void {
    this.adding = false;
    this.addSearch = '';
    const opened = new Set(this.opened);
    opened.delete(column);
    this.opened = opened;
    const s = this.resolvedStore;
    if (s && column.startsWith(CLUSTER_PREFIX)) {
      const layer = column.slice(CLUSTER_PREFIX.length);
      const members = s.get('filters').members;
      if (members.some((m) => m.layer === layer)) s.setMembers(members.filter((m) => m.layer !== layer));
    } else if (s) {
      const draft = s.get('filters').draft;
      const held = MODES.map(([verb]) => verb).filter((verb) => {
        const d = draft[verb][column];
        return d !== undefined && isPopulated(d);
      });
      if (held.length > 0) {
        s.setFilters(held.reduce((d, verb) => withoutClause(d, column, verb), draft));
        for (const verb of held) emit(this, 'tessera-filterchange', {column, verb, expr: null});
      }
    }
    void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="add"]')?.focus());
  }

  /** The arrow keys, Home and End move among the Add filter options; Up from the first goes to the search box. */
  private addKeys(e: KeyboardEvent, i: number | null): void {
    const items = Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="add-option"]'));
    if (items.length === 0) return;
    const last = items.length - 1;
    const next =
      i === null
        ? {ArrowDown: 0}[e.key]
        : {ArrowDown: Math.min(i + 1, last), ArrowUp: i - 1, Home: 0, End: last}[e.key];
    if (next === undefined) return;
    e.preventDefault();
    if (next < 0) {
      this.renderRoot.querySelector<HTMLElement>('[part="add-search"]')?.focus();
      return;
    }
    this.addActive = next;
    items[next]?.focus();
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel"><h2 part="title">Filters</h2>${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const operands = meta.filterOperands;
    if (operands.length === 0) return html`<div class="panel"><h2 part="title">Filters</h2><span part="state" data-state="empty">Nothing filterable</span></div>`;
    const {draft, members} = s.get('filters');
    const active = activeCount(draft);
    const inPosition = (verb: ClauseVerb) => activeCount(draft, verb) + members.filter((m) => m.verb === verb).length;
    const holds = (column: string, verb: ClauseVerb) => {
      const d = draft[verb][column];
      return d !== undefined && isPopulated(d);
    };
    // A column's filter chip and then its highlight chip, the columns in the draft's order.
    const columns = [...new Set([...Object.keys(draft.filter), ...Object.keys(draft.highlight)])];
    const chips = columns.flatMap((c) => (['filter', 'highlight'] as const).flatMap((verb) => (holds(c, verb) ? [{c, verb, d: draft[verb][c]!}] : [])));
    if (this.chipsOnly && chips.length === 0 && members.length === 0) return nothing;
    const chipList =
      chips.length > 0 || members.length > 0
        ? html`<div part="chips">
            ${chips.map(
              ({c, verb, d}) => html`<span part="chip" class="chip" data-verb=${verb} data-column=${c}
                ><button part="edit" class="edit" type="button" title=${`Edit the ${verb}`} @click=${() => this.edit(c, verb)}>${this.mark(verb)}${this.chipText(c, d)}</button
                ><button type="button" aria-label=${`Remove the ${columnCaption(c)} ${verb}`} @click=${() => this.clearColumn(c, verb)}>${icon('close', 12)}</button></span
              >`
            )}
            ${members.map((m) => this.memberChip(m))}
          </div>`
        : nothing;
    const clear = active > 0 || members.length > 0 ? html`<button part="clear" class="quiet" type="button" @click=${() => this.clearAll()}>Clear all</button>` : nothing;
    if (this.chipsOnly) {
      return html`<div class="panel"><h2 part="title">Filters${clear}</h2>
          <span part="state" data-state="shown"></span>${chipList}
        </div>`;
    }

    // Every field: the filterable columns, then the layers whose clusters can be filtered by.
    const layers = clusterFieldLayers(meta, s.get('view').id);
    const fields = [
      ...operands.map((o) => ({key: o.column, title: columnCaption(o.column), layer: null as Layer | null})),
      ...layers.map((l) => ({key: layerField(l.name), title: l.title || l.name, layer: l as Layer | null}))
    ];
    const pinned = new Set(this.pinned.split(/[\s,]+/).filter(Boolean));
    const holdsField = (f: (typeof fields)[number], verb: ClauseVerb) => (f.layer ? members.some((m) => m.layer === f.layer!.name && m.verb === verb) : holds(f.key, verb));
    const listed = fields.filter((f) => pinned.has(f.key) || this.opened.has(f.key) || holdsField(f, 'filter') || holdsField(f, 'highlight'));
    const at = MODES.findIndex(([v]) => v === this.mode);
    const heading = this.controlsOnly ? nothing : html`<div class="head top"><h2 part="title">Filters${clear}</h2></div><span part="state" data-state="shown"></span>`;
    const modeSwitch = html`<div class="head mode-row"><div part="mode" role="radiogroup" aria-label="Edit">
      ${MODES.map(([v, t], i) => {
        const n = inPosition(v);
        return html`<button type="button" role="radio" data-verb=${v} aria-checked=${this.mode === v ? 'true' : 'false'} tabindex=${i === at ? '0' : '-1'}
          aria-label=${`${t}, ${n} applied`}
          @click=${() => this.chooseMode(v)} @keydown=${(e: KeyboardEvent) => radioKeys(e, MODES.length, i, (j) => this.chooseMode(MODES[j]![0]))}
          >${icon(v === 'filter' ? 'filter' : 'highlight', 13, 1.4)}${t}${n > 0 ? html`<span part="mode-count">${n}</span>` : nothing}</button>`;
      })}
    </div></div>`;
    const field = (f: (typeof fields)[number]) => {
      const open = holdsField(f, this.mode) || this.opened.has(f.key);
      const control = f.layer
        ? html`<tessera-cluster-filter exportparts=${CLUSTER_PARTS} layer=${f.layer.name} .verb=${this.mode} .store=${s}></tessera-cluster-filter>`
        : html`<tessera-filter exportparts=${FILTER_PARTS} column=${f.key} .verb=${this.mode} .store=${s}></tessera-filter>`;
      // A control the user has changed stays open, even as its clause empties under them.
      return html`<div part="field" data-column=${f.layer ? nothing : f.key} data-layer=${f.layer ? f.layer.name : nothing} ?data-open=${open}
        @tessera-filterchange=${() => this.keepOpen(f.key)} @tessera-clausechange=${() => this.keepOpen(f.key)}>
        ${open ? control : html`<button part="any" type="button" @click=${() => this.show(f.key, this.mode)}><span class="n">${f.title}</span><span class="v">Any</span></button>`}
      </div>`;
    };
    const q = this.addSearch.trim().toLowerCase();
    const offered = fields.filter((f) => q === '' || f.key.toLowerCase().includes(q) || f.title.toLowerCase().includes(q));
    const tabbed = this.addActive < offered.length ? this.addActive : 0;
    const choose = (key: string) => {
      if (!listed.some((f) => f.key === key)) this.add(key);
      else if (!pinned.has(key)) this.takeOff(key);
    };
    const adder = html`<div class="adder">
            <button part="add" class="btn" type="button" aria-expanded=${this.adding ? 'true' : 'false'} aria-controls="add-list"
              @click=${() => {
                this.adding = !this.adding;
                this.addSearch = '';
                this.addActive = 0;
              }}>${icon('plus', 14, 1.4)}Add filter</button><div class="anchor"></div>
            ${this.adding
              ? html`<div part="add-list" id="add-list" popover="manual" @keydown=${(e: KeyboardEvent) => {
                  if (e.key !== 'Escape') return;
                  e.stopPropagation();
                  this.adding = false;
                  this.renderRoot.querySelector<HTMLElement>('[part="add"]')?.focus();
                }}>
                  <div class="input">${icon('search', 14)}<input part="add-search" type="search" autocomplete="off" placeholder="Find a field" aria-label="Find a field to filter" .value=${this.addSearch}
                    @input=${(e: Event) => {
                      this.addSearch = (e.target as HTMLInputElement).value;
                      this.addActive = 0;
                    }}
                    @keydown=${(e: KeyboardEvent) => {
                      // Enter adds the first match not yet listed. Taking a field off is always a press on its row.
                      if (e.key !== 'Enter') return this.addKeys(e, null);
                      e.preventDefault();
                      const unlisted = offered.find((o) => !listed.includes(o));
                      if (unlisted) this.add(unlisted.key);
                    }} /></div>
                  ${offered.length > 0
                    ? html`<div role="menu" aria-label="Fields">${offered.map((o, i) => {
                        const added = listed.includes(o);
                        const kept = added && pinned.has(o.key);
                        return html`<button part="add-option" type="button" role="menuitemcheckbox" data-column=${o.layer ? nothing : o.key} data-layer=${o.layer ? o.layer.name : nothing}
                          aria-checked=${added ? 'true' : 'false'}
                          aria-disabled=${kept ? 'true' : nothing} title=${kept ? 'Always shown here' : added ? 'Remove from the panel' : nothing} tabindex=${i === tabbed ? '0' : '-1'}
                          @click=${() => choose(o.key)} @keydown=${(e: KeyboardEvent) => this.addKeys(e, i)}><span>${o.title}</span>${o.layer ? html`<span class="kind">Clusters</span>` : nothing}</button>`;
                      })}</div>`
                    : html`<span class="none">No field by that name</span>`}
                </div>`
              : nothing}
          </div>`;
    // A clause on a layer with no field here (one that attaches to another, or one of another view)
    // is still applied, so it is shown and can be taken off.
    const others = members.filter((m) => !layers.some((l) => l.name === m.layer));
    const otherList =
      others.length > 0
        ? html`<div part="others" class="others"><span class="n">Also applied</span><div part="chips">${others.map((m) => this.memberChip(m))}</div></div>`
        : nothing;
    return html`${heading}${modeSwitch}${repeat(
      listed,
      (f) => f.key,
      (f) => field(f)
    )}${otherList}${adder}`;
  }

  /** A `member_of` clause as a chip whose × takes it off. */
  private memberChip(m: MemberClause): TemplateResult {
    return html`<span part="chip" class="chip" data-verb=${m.verb} data-artifact=${String(m.artifact)}
      >${this.mark(m.verb)}${this.memberText(m)}<button type="button" aria-label=${`Remove ${this.memberText(m)}`} @click=${() => this.clearMember(m)}>${icon('close', 12)}</button></span
    >`;
  }

  protected override willUpdate(changed: PropertyValues<this>): void {
    super.willUpdate(changed);
    // The search box takes focus as the list opens.
    if (changed.has('adding') && this.adding) void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="add-search"]')?.focus());
  }
}

attachContextRoot();
defineOnce('tessera-filter-panel', TesseraFilterPanel);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-filter-panel': TesseraFilterPanel;
  }
}
