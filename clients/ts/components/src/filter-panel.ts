import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {activeCount, artifactName, emptyDraft, isPopulated, withoutClause, withoutMember, type ClauseVerb, type ColumnDraft, type MemberClause} from '@tesseradb/client';
import {OPERATOR_WORDS, type TesseraFilter} from './filter.js';
import {TesseraElement, UNNAMED, columnCaption, dateRangeText, emit, keyTitle} from './base.js';
import {radioKeys} from './display.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {exportparts} from './parts.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';
import './filter.js';

/** Each control's parts, forwarded as `filter-<part>` (`parts.ts`). */
const FILTER_PARTS = exportparts('filter');

const MODES: [ClauseVerb, string][] = [
  ['filter', 'Filter'],
  ['highlight', 'Highlight']
];

/**
 * The clauses applied, as chips under a Clear all button, and the filter controls under a Filter /
 * Highlight switch. A clause is in one of two positions: a filter narrows the map and every count
 * to the matches, and a highlight lights the matches among what the filter keeps. A column or an
 * artifact can hold a clause in each. The two are independent: neither changes the other, and each
 * has its own chip. A highlight chip carries the highlight mark and colour; a filter chip carries
 * none. A `member_of` clause (an artifact chosen on the artifact card or in the hierarchy) is a
 * chip too.
 *
 * The switch sets `mode`, the position every control edits. The controls listed are the columns in
 * `pinned` and those holding a clause in either position, in `meta`'s order, and any the user has
 * opened or changed. A listed column shows its `<tessera-filter>` while it holds a clause in the
 * current position or the user opened or changed it, and otherwise a row reading "Any" that opens
 * it. Add filter lists every filterable column, with a search box, and the listed ones are checked.
 * Choosing an unchecked column lists and opens it. Choosing a checked one takes it off the panel and
 * empties its clause in both positions; a column in `pinned` stays, since the host lists it.
 *
 * Pressing a column's chip sets `mode` to the chip's position and opens, scrolls to and focuses
 * the column's control; under `chips-only`, where there are no controls, it fires
 * `tessera-chipopen` instead, for a host to show them with {@link TesseraFilterPanel.show}.
 * Removing a chip empties its clause and leaves the other position's alone. Clear all empties
 * every control in both positions and drops every `member_of` clause. `chips-only` leaves the
 * controls out, and renders nothing while no clause is applied; `controls-only` leaves the chips
 * out.
 *
 * @summary The applied clauses as chips, and the filter controls.
 * @tagname tessera-filter-panel
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A
 *   column's chip was removed (with `verb`, the position it was in, and `expr` null), or Clear all
 *   was pressed (with `column` and `expr` null). Removing a `member_of` chip fires nothing. Each
 *   inner control fires its own as well.
 * @fires {CustomEvent<TesseraEventDetails['tessera-chipopen']>} tessera-chipopen - A column's chip
 *   was pressed under `chips-only`, naming the column and the position its control is to edit.
 * @csspart title - The heading over the chips, holding Clear all at its right.
 * @csspart clear - The Clear all button, shown while any clause is applied.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - The words "View refused", with `data-code`, in the refused state.
 * @csspart chips - The applied clauses.
 * @csspart chip - One applied clause, with `data-verb` (`filter` or `highlight`) and `data-column`
 *   or `data-artifact`.
 * @csspart edit - The button that is a column chip's text, which opens its control.
 * @csspart verb - The highlight mark on a highlight chip.
 * @csspart mode - The Filter / Highlight switch: two radio buttons with `data-verb` and
 *   `aria-checked`.
 * @csspart field - A listed column's section, with `data-column`, and `data-open` while its control
 *   shows.
 * @csspart any - A closed column's row, the button that opens its control.
 * @csspart add - The Add filter button, with `aria-expanded`.
 * @csspart add-list - The list of columns to add, while it is open.
 * @csspart add-search - The search box over that list.
 * @csspart add-option - One column in that list, with `data-column`, `aria-checked` while it is
 *   listed, and `aria-disabled` where it is pinned and so cannot be taken off.
 * @csspart filter-<part> - A part of an inner `<tessera-filter>`, forwarded under a `filter-`
 *   prefix: `filter-entry`, `filter-tick`, and so on.
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
      [part='mode'] {
        display: flex;
        padding: 2px;
        gap: 2px;
        background: var(--_tessera-surface-3);
        border-radius: 7px;
      }
      [part='mode'] button {
        display: flex;
        align-items: center;
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
      .adder {
        padding: 10px var(--_tessera-panel-inline, 16px) 12px;
      }
      [part='add'] {
        height: auto;
        padding: 5px 10px;
      }
      [part='add-list'] {
        margin-top: 8px;
        padding: 4px;
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius-control);
        box-shadow: 0 6px 18px rgba(0, 0, 0, 0.08);
      }
      [part='add-list'] .input {
        margin-bottom: 4px;
      }
      [part~='add-option'] {
        display: block;
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
    if (!this.editing || this.chipsOnly) return;
    const control = Array.from(this.renderRoot.querySelectorAll<TesseraFilter>('tessera-filter')).find((f) => f.column === this.editing);
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

  /** Take a column off the panel, emptying its clause in both positions. */
  private takeOff(column: string): void {
    this.adding = false;
    this.addSearch = '';
    const opened = new Set(this.opened);
    opened.delete(column);
    this.opened = opened;
    const s = this.resolvedStore;
    if (s) {
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
    items.forEach((item, j) => (item.tabIndex = j === next ? 0 : -1));
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
            ${members.map(
              (m) => html`<span part="chip" class="chip" data-verb=${m.verb} data-artifact=${String(m.artifact)}
                >${this.mark(m.verb)}${this.memberText(m)}<button type="button" aria-label=${`Remove ${this.memberText(m)}`} @click=${() => this.clearMember(m)}>${icon('close', 12)}</button></span
              >`
            )}
          </div>`
        : nothing;
    const summary = this.controlsOnly
      ? nothing
      : html`<div class="panel"><h2 part="title">Filters${active > 0 || members.length > 0 ? html`<button part="clear" class="quiet" type="button" @click=${() => this.clearAll()}>Clear all</button>` : nothing}</h2>
          <span part="state" data-state="shown"></span>${chipList}${chips.length === 0 && members.length === 0 ? html`<span class="faint sm">None</span>` : nothing}
        </div>`;
    if (this.chipsOnly) return summary;

    const pinned = new Set(this.pinned.split(/[\s,]+/).filter(Boolean));
    const listed = operands.filter((o) => pinned.has(o.column) || this.opened.has(o.column) || holds(o.column, 'filter') || holds(o.column, 'highlight'));
    const at = MODES.findIndex(([v]) => v === this.mode);
    const modeSwitch = html`<div class="head"><div part="mode" role="radiogroup" aria-label="Edit">
      ${MODES.map(
        ([v, t], i) => html`<button type="button" role="radio" data-verb=${v} aria-checked=${this.mode === v ? 'true' : 'false'} tabindex=${i === at ? '0' : '-1'}
          @click=${() => this.chooseMode(v)} @keydown=${(e: KeyboardEvent) => radioKeys(e, MODES.length, i, (j) => this.chooseMode(MODES[j]![0]))}
          >${icon(v === 'filter' ? 'filter' : 'highlight', 13, 1.4)}${t}</button>`
      )}
    </div></div>`;
    const field = (column: string) => {
      const open = holds(column, this.mode) || this.opened.has(column);
      // A control the user has changed stays open, even as its clause empties under them.
      return html`<div part="field" data-column=${column} ?data-open=${open} @tessera-filterchange=${() => this.keepOpen(column)}>
        ${open
          ? html`<tessera-filter exportparts=${FILTER_PARTS} column=${column} .verb=${this.mode} .store=${s}></tessera-filter>`
          : html`<button part="any" type="button" @click=${() => this.show(column, this.mode)}><span class="n">${columnCaption(column)}</span><span class="v">Any</span></button>`}
      </div>`;
    };
    const q = this.addSearch.trim().toLowerCase();
    const offered = operands.filter((o) => q === '' || o.column.toLowerCase().includes(q) || columnCaption(o.column).toLowerCase().includes(q));
    const choose = (column: string) => {
      if (!listed.some((o) => o.column === column)) this.add(column);
      else if (!pinned.has(column)) this.takeOff(column);
    };
    const adder = html`<div class="adder">
            <button part="add" class="btn" type="button" aria-expanded=${this.adding ? 'true' : 'false'} aria-controls="add-list"
              @click=${() => {
                this.adding = !this.adding;
                this.addSearch = '';
              }}>${icon('plus', 14, 1.4)}Add filter</button>
            ${this.adding
              ? html`<div part="add-list" id="add-list" @keydown=${(e: KeyboardEvent) => {
                  if (e.key !== 'Escape') return;
                  e.stopPropagation();
                  this.adding = false;
                  this.renderRoot.querySelector<HTMLElement>('[part="add"]')?.focus();
                }}>
                  <div class="input">${icon('search', 14)}<input part="add-search" type="search" autocomplete="off" placeholder="Find a field" aria-label="Find a field to filter" .value=${this.addSearch}
                    @input=${(e: Event) => (this.addSearch = (e.target as HTMLInputElement).value)}
                    @keydown=${(e: KeyboardEvent) => (e.key === 'Enter' && offered[0] ? choose(offered[0].column) : this.addKeys(e, null))} /></div>
                  ${offered.length > 0
                    ? html`<div role="menu" aria-label="Fields">${offered.map((o, i) => {
                        const added = listed.includes(o);
                        const kept = added && pinned.has(o.column);
                        return html`<button part="add-option" type="button" role="menuitemcheckbox" data-column=${o.column} aria-checked=${added ? 'true' : 'false'}
                          aria-disabled=${kept ? 'true' : nothing} title=${kept ? 'Always shown here' : added ? 'Remove from the panel' : nothing} tabindex=${i === 0 ? '0' : '-1'}
                          @click=${() => choose(o.column)} @keydown=${(e: KeyboardEvent) => this.addKeys(e, i)}>${columnCaption(o.column)}</button>`;
                      })}</div>`
                    : html`<span class="none">No field by that name</span>`}
                </div>`
              : nothing}
          </div>`;
    return html`${summary}${modeSwitch}${repeat(
      listed,
      (o) => o.column,
      (o) => field(o.column)
    )}${adder}`;
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
