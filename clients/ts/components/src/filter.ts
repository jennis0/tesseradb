import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {composeFilters, emptyDraft, isPopulated, type ClauseVerb, type ColumnDraft, type FilterOperandSet, type MatchSpan, type Refusal, type SuggestionPage, type SuggestValue} from '@tesseradb/client';
import {HeldAggregate, listedGroups, type GroupCount} from './aggregate.js';
import {TesseraElement, columnCaption, emit, keyTitle, parseDateText, shortDateText} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {chrome, tokens} from './tokens.js';

/** The operators a keyword control can send. */
const KEYWORD_OPERATORS = ['contains', 'prefix', 'eq'] as const;
type KeywordOperator = (typeof KEYWORD_OPERATORS)[number];
/** Each operator as the select and a chip say it. */
export const OPERATOR_WORDS: Record<KeywordOperator, string> = {contains: 'contains', prefix: 'starts with', eq: 'is'};

/** How long a typed control must be quiet before its change is sent. */
const TYPING_DEBOUNCE_MS = 350;

/** The narrowest bar a suggested value with any items draws, as a percentage, so it still shows. */
const BAR_FLOOR = 2;

/** How many of a category's commonest values show under its box before anything is typed. */
const TOP_VALUES = 5;

/**
 * One filter control for the column `column`, drawn by the family `/v1/meta` gives the column, under
 * the column's name. `verb` is the position it edits: the column's filter clause or its highlight
 * clause. The two are separate clauses; the control shows the one in its position and leaves the
 * other alone.
 *
 * A text column is one search box. Its words must all appear; words in double quotes are a phrase,
 * or plain words where the column takes no phrase; `OR` between terms asks for either. A line under
 * the box says so. A text clause set from outside that no query writes shows read-only, with Clear.
 *
 * A category is a search box over `/v1/categories/{column}/suggest`, with the five commonest values
 * in the current set under it before anything is typed, each with a checkbox, its exact count and
 * a bar, its share of the set. The five are counted by the aggregate route (`Store.setAggregate`):
 * in the filter position without the column's own clause, so a value its clause excludes is still
 * counted, and in the highlight position under the whole filter. The control keeps that aggregate
 * registered while it is drawn. The suggestions open over what sits below the box. Each value suggested shows its count in the current view among the items
 * passing the filter, and a bar, its share of the total the server counted over. In the filter
 * position the count leaves out the column's own clause, so a value counts what choosing it as well
 * would add. In the highlight position the count is under the whole filter, and a value counted 0
 * is greyed, marked "none match" and cannot be chosen, though one already chosen can be taken out.
 * The arrow keys move through the suggestions, and Enter chooses the one reached, the first by
 * default, or takes it out where it is chosen; text that suggests nothing chooses nothing. Emptying
 * the box, or removing the control, has the store stop asking for the column. The values chosen
 * that are not among the five sit under them as chips, each with a ×. The heading says how many
 * values the current set holds, from the same aggregate, or where it has not answered, how many
 * the legend holds.
 *
 * A number is two inputs, and a date two text inputs that read and write dates as day, month and
 * year (`1 Jan 2019`); a date typed as a month or a year means its first day in the lower input
 * and its last in the upper. A keyword column is a text box with its operator (`contains`,
 * `prefix` or `eq`).
 *
 * Typing is sent 350 ms after the last keystroke; a choice is sent at once, and so is typing still
 * waiting when the position changes. Each change replaces the column's control in the control's
 * position of the store's draft (`Store.setFilters`). The host carries `data-on` while the control
 * holds a value.
 *
 * @summary One filter control, drawn by the column's type.
 * @tagname tessera-filter
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - The
 *   control changed, with the column, its position and the expression that position composes.
 * @csspart label - The column's name, as a caption.
 * @csspart aside - The heading's right-hand text: how many values a category has, or a number or
 *   date control's Clear button.
 * @csspart entry - A text, number or date input, with `aria-invalid` on a date that does not read.
 * @csspart hint - The line under a text column's box saying how to write a query.
 * @csspart mode - The keyword column's operator select.
 * @csspart values - The typeahead's suggestions.
 * @csspart tick - One suggested value, with `aria-selected`, and `aria-disabled` in the highlight
 *   position where it is counted 0 and not chosen.
 * @csspart bar - A suggested value's share of the items its count is taken over.
 * @csspart value-count - A suggested value's count.
 * @csspart top - The commonest values, under the box while nothing is typed.
 * @csspart top-value - One of them: a label holding its checkbox, with `data-key`.
 * @csspart chosen - A chosen category value's chip.
 * @csspart more - The hint that more values match than one page holds.
 * @csspart refusal - The words "Values unavailable" where the values could not be listed, with
 *   `data-code` set to the refusal's code.
 */
export class TesseraFilter extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      .head {
        display: flex;
        align-items: baseline;
        justify-content: space-between;
        gap: 8px;
        margin-bottom: 8px;
      }
      [part='label'] {
        display: block;
        font-weight: 600;
        color: var(--_tessera-ink);
      }
      [part='aside'] {
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
      button[part='aside'] {
        font-weight: 500;
        color: var(--_tessera-ink-2);
      }
      :host([verb='highlight']) .input:focus-within {
        outline: 0;
        border: 1.5px solid var(--_tessera-highlight);
        padding: 0 9.5px;
      }
      .ctl-row {
        display: flex;
        align-items: center;
        gap: 6px;
      }
      .ctl-row .grow {
        flex: 1 1 0;
        min-width: 0;
      }
      .range {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr);
        gap: 8px;
        align-items: center;
      }
      .range input {
        width: 100%;
        height: 28px;
        padding: 0 8px;
        font-variant-numeric: tabular-nums;
      }
      .range input[aria-invalid='true'] {
        border-color: var(--_tessera-refuse);
      }
      [part='hint'] {
        display: block;
        margin-top: 6px;
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
      .combo {
        position: relative;
      }
      /* The suggestions open over what sits below the box. */
      [part='values'] {
        position: absolute;
        z-index: 5;
        top: calc(100% + 4px);
        left: 0;
        right: 0;
        max-height: 280px;
        overflow-y: auto;
        display: flex;
        flex-direction: column;
        padding: 4px;
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius-control);
        background: var(--_tessera-surface);
        box-shadow: 0 6px 18px rgba(0, 0, 0, 0.08);
      }
      [part~='tick'] {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto;
        align-items: center;
        column-gap: 10px;
        padding: 5px 8px;
        border-radius: 4px;
        text-align: left;
      }
      [part~='tick']:hover,
      [part~='tick'][data-active] {
        background: var(--_tessera-surface-2);
      }
      [part~='tick'][aria-selected='true'] {
        background: var(--_tessera-surface-2);
        font-weight: 600;
      }
      :host([verb='highlight']) [part~='tick'][aria-selected='true'] {
        background: var(--_tessera-highlight-soft);
        color: var(--_tessera-highlight);
      }
      [part~='tick'][aria-disabled='true'] {
        cursor: default;
        background: none;
        color: var(--_tessera-ink-3);
      }
      .opt {
        display: flex;
        flex-direction: column;
        gap: 3px;
        min-width: 0;
      }
      .opt .t {
        display: flex;
        align-items: baseline;
        gap: 6px;
        min-width: 0;
      }
      .opt .name {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      .opt .out {
        flex: none;
        font-size: 11px;
        font-weight: 400;
        color: var(--_tessera-ink-3);
      }
      /* The matched span the server gave. */
      [part~='tick'] mark {
        background: none;
        color: inherit;
        font-weight: 600;
      }
      /* The key follows the title, muted, since a key may be an opaque identifier such as a uuid. */
      [part~='tick'] .k {
        margin-left: 0.45em;
        opacity: 0.55;
        font-size: 0.85em;
        font-variant-numeric: tabular-nums;
      }
      .track {
        height: 3px;
        border-radius: 2px;
        background: var(--_tessera-surface-3);
      }
      [part='bar'] {
        height: 3px;
        border-radius: 2px;
        background: color-mix(in srgb, var(--_tessera-ink-2) 75%, var(--_tessera-surface));
      }
      [aria-selected='true'] [part='bar'] {
        background: var(--_tessera-ink);
      }
      :host([verb='highlight']) [aria-selected='true'] [part='bar'] {
        background: var(--_tessera-highlight);
      }
      [part='value-count'] {
        font-size: 12px;
        font-weight: 400;
        font-variant-numeric: tabular-nums;
        color: var(--_tessera-ink-2);
      }
      [aria-disabled='true'] [part='value-count'] {
        color: var(--_tessera-ink-3);
      }
      [part='top'] {
        display: flex;
        flex-direction: column;
        gap: 6px;
        margin-top: 10px;
      }
      [part~='top-value'] {
        display: grid;
        grid-template-columns: 16px minmax(0, 1fr) auto;
        align-items: center;
        column-gap: 8px;
        cursor: pointer;
      }
      [part~='top-value'] input {
        width: 14px;
        height: 14px;
        margin: 0;
        accent-color: var(--_tessera-ink);
      }
      :host([verb='highlight']) [part~='top-value'] input {
        accent-color: var(--_tessera-highlight);
      }
      [part~='top-value'] input:checked ~ .opt [part='bar'] {
        background: var(--_tessera-ink);
      }
      :host([verb='highlight']) [part~='top-value'] input:checked ~ .opt [part='bar'] {
        background: var(--_tessera-highlight);
      }
      .chosen {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
        margin-top: 8px;
      }
      [part='more'] {
        display: block;
        margin-top: 6px;
        color: var(--_tessera-ink-3);
        font-size: 12px;
      }
      [part='refusal'] {
        display: flex;
        align-items: center;
        gap: 8px;
        margin-top: 6px;
        font-size: 12px;
      }
      .skel {
        margin-top: 8px;
      }
      .to {
        color: var(--_tessera-ink-3);
      }
    `
  ];

  /** The column this control filters, a name `meta.filterOperands` lists. Unset or unknown, the control renders nothing. */
  @property() accessor column = '';
  /** The column's operands, for a host that sets them itself in place of the store's `meta`. */
  @property({attribute: false}) accessor operand: FilterOperandSet | null = null;
  /** The position the control edits: the column's `filter` clause or its `highlight` clause. */
  @property({reflect: true}) accessor verb: ClauseVerb = 'filter';

  /** @internal */
  @state() accessor draft: ColumnDraft | null = null;
  /** @internal */
  @state() accessor search = '';
  /** The suggestion the arrow keys moved to, by code; `null` is the first that can be chosen. @internal */
  @state() accessor activeCode: number | null = null;
  /** The date inputs whose text did not read as a date, which keep the text typed. @internal */
  @state() accessor invalid: {gte?: string; lte?: string} = {};
  private sent: ColumnDraft | null = null;
  /** The commonest values under a category's box. */
  private readonly top = new HeldAggregate('filter');
  private typing: ReturnType<typeof setTimeout> | null = null;
  /** The change `typing` is waiting to send, so a change of position can send it first. */
  private pending: (() => void) | null = null;
  /**
   * The last `q` this element asked the store's typeahead for. Store changes arrive for every
   * projection, and re-asking on each would keep re-arming the store's debounce so no request went
   * out; asking only on a new `q` avoids that.
   */
  private lastAsked: string | null = null;
  /**
   * The store's `filters.suggestEpoch` as last seen, `-1` before the first update. A change means
   * every held page was invalidated (a view switch or re-authorisation), so a typed `q` is asked
   * again.
   */
  private lastEpoch = -1;

  protected override resetServerData(): void {
    this.search = '';
    this.lastAsked = null;
    this.lastEpoch = -1;
    // The draft being edited was the previous store's; the next is seeded from the one adopted.
    this.draft = null;
    this.sent = null;
  }

  private get resolvedOperand(): FilterOperandSet | null {
    if (this.operand) return this.operand;
    return this.resolvedStore?.get('meta')?.filterOperands.find((o) => o.column === this.column) ?? null;
  }

  /** The suggestion page for `this.search` in this position, or `null` while it is stale or has not landed. */
  private get resolvedSuggestion(): SuggestionPage | null {
    const s = this.resolvedStore?.get('filters').suggestions[this.column];
    return s && s.q === this.search && s.verb === this.verb ? s : null;
  }

  private get resolvedSuggestRefusal(): Refusal | null {
    return this.resolvedStore?.get('filters').suggestErrors[this.column] ?? null;
  }

  /**
   * The number the suggestion counts are counted over, which a value's bar is a share of: the
   * page's `total`. `null` where the page has none or it is 0.
   */
  private countedOver(page: SuggestionPage | null): number | null {
    return page?.total ? page.total : null;
  }

  /**
   * Ask the store's typeahead for `q`, once per distinct `q`. An emptied box asks nothing and has
   * the store forget the column, so a change of filter does not ask for it.
   */
  private ask(q: string): void {
    const s = this.resolvedStore;
    if (!s || this.lastAsked === q) return;
    if (q === '') {
      if (this.lastAsked !== null) s.forgetSuggestions(this.column);
      this.lastAsked = null;
      return;
    }
    this.lastAsked = q;
    s.suggest(this.column, q, this.verb);
  }

  override disconnectedCallback(): void {
    this.ask('');
    this.top.set(null, null);
    super.disconnectedCallback();
  }

  protected override onStoreChange(): void {
    // Re-seed from the store only when its draft changed underneath (a clear all, or the first
    // meta), not while the user's own edit is in flight.
    const stored = this.resolvedStore?.get('filters').draft[this.verb][this.column] ?? null;
    if (stored && stored !== this.sent && JSON.stringify(stored) !== JSON.stringify(this.draft)) {
      this.draft = structuredClone(stored);
      this.sent = stored;
    }
    if (this.resolvedOperand?.family === 'category') {
      const epoch = this.resolvedStore?.get('filters').suggestEpoch ?? -1;
      if (epoch !== this.lastEpoch) {
        this.lastEpoch = epoch;
        this.lastAsked = null;
      }
      this.ask(this.search);
    }
    super.onStoreChange();
  }

  protected override willUpdate(changed: PropertyValues<this>): void {
    super.willUpdate(changed);
    // A new position shows that position's clause, once any typing for the old one is sent.
    if (changed.has('verb') && changed.get('verb') !== undefined) {
      if (this.typing) clearTimeout(this.typing);
      this.pending?.();
      this.draft = null;
      this.sent = null;
      // The other position counts under another filter.
      this.lastAsked = null;
      this.ask(this.search);
      this.invalid = {};
    }
  }

  /** Moves focus to the control's first input. */
  override focus(options?: FocusOptions): void {
    const target = this.renderRoot.querySelector<HTMLElement>('#ctl');
    if (target) target.focus(options);
    else super.focus(options);
  }

  protected override updated(): void {
    if (this.draft && isPopulated(this.draft)) this.setAttribute('data-on', '');
    else this.removeAttribute('data-on');
    const category = this.isConnected && this.resolvedOperand?.family === 'category';
    this.top.set(
      this.resolvedStore,
      category ? {groupings: [{by: {field: this.column, top: TOP_VALUES}}], ...(this.verb === 'filter' ? {without: this.column} : {})} : null
    );
  }

  /** The commonest values and the number of values in the set, once the aggregate has answered. */
  private topValues(): {values: GroupCount[]; total: number; groups: number | null} | null {
    const table = this.top.entry()?.result?.tables[0];
    return table ? {values: listedGroups(table), total: table.total, groups: table.groups} : null;
  }

  /**
   * The control's draft: the one being edited, else the store's, else the client's empty draft for
   * this operand, which also sets a keyword control's starting operator.
   */
  private currentDraft(o: FilterOperandSet): ColumnDraft | null {
    return this.draft ?? this.resolvedStore?.get('filters').draft[this.verb][this.column] ?? emptyDraft([o]).filter[o.column] ?? null;
  }

  private change(next: ColumnDraft, immediate: boolean): void {
    this.draft = next;
    if (this.typing) clearTimeout(this.typing);
    const {column, verb} = this;
    const apply = () => {
      this.typing = null;
      this.pending = null;
      const s = this.resolvedStore;
      if (!s) return;
      const held = s.get('filters').draft;
      const draft = {...held, [verb]: {...held[verb], [column]: next}};
      this.sent = next;
      s.setFilters(draft);
      emit(this, 'tessera-filterchange', {column, verb, expr: composeFilters(draft, verb)});
    };
    if (immediate) apply();
    else {
      this.pending = apply;
      this.typing = setTimeout(apply, TYPING_DEBOUNCE_MS);
    }
  }

  override render(): TemplateResult | typeof nothing {
    const o = this.resolvedOperand;
    if (!o) return nothing;
    const draft = this.currentDraft(o);
    if (!draft) return nothing;
    return html`<div class="head"><label part="label" for="ctl">${columnCaption(this.column)}</label>${this.aside(draft)}</div>${this.body(o, draft)}`;
  }

  /** The heading's right-hand side: a category's number of values, or Clear on a range. */
  private aside(draft: ColumnDraft): TemplateResult | typeof nothing {
    if (draft.family === 'category') {
      const n = this.topValues()?.groups ?? this.resolvedStore?.get('legend').categories[this.column]?.length ?? null;
      return n === null ? nothing : html`<span part="aside">${n.toLocaleString('en-GB')} ${n === 1 ? 'value' : 'values'}</span>`;
    }
    if (draft.family === 'numeric' && isPopulated(draft)) {
      return html`<button part="aside" type="button" @click=${() => {
        this.invalid = {};
        this.change({family: 'numeric', gte: null, lte: null}, true);
      }}>Clear</button>`;
    }
    if (draft.family === 'text' && draft.expr !== undefined) {
      return html`<button part="aside" type="button" @click=${() => this.change({family: 'text', query: '', phrase: draft.phrase}, true)}>Clear</button>`;
    }
    return nothing;
  }

  private body(o: FilterOperandSet, draft: ColumnDraft) {
    switch (draft.family) {
      case 'text':
        return this.text(draft);
      case 'keyword':
        return this.keyword(o, draft);
      case 'category':
        return this.category(draft);
      case 'numeric':
        return this.numeric(draft);
    }
  }

  private text(draft: ColumnDraft & {family: 'text'}) {
    // An expression set from outside that no query writes is shown as it is, and cannot be typed over.
    if (draft.expr !== undefined) {
      return html`<div class="input">${icon('search', 14)}<input id="ctl" part="entry" readonly .value=${JSON.stringify(draft.expr)} aria-describedby="hint" /></div>
        <span part="hint" id="hint">Set from outside. Clear it to type a search.</span>`;
    }
    const hint = draft.phrase ? 'Words match together. Use “quotes” for a phrase, OR for either.' : 'Words match together. Use OR for either.';
    return html`<div class="input">${icon('search', 14)}<input id="ctl" part="entry" type="search" .value=${draft.query} placeholder="Search the text" autocomplete="off"
        aria-describedby="hint"
        @input=${(e: Event) => this.change({family: 'text', query: (e.target as HTMLInputElement).value, phrase: draft.phrase}, false)} /></div>
      <span part="hint" id="hint">${hint}</span>`;
  }

  /** A keyword control, offering the operators the column publishes. */
  private keyword(o: FilterOperandSet, draft: ColumnDraft & {family: 'keyword'}) {
    const ops = o.operands.filter((op): op is KeywordOperator => (KEYWORD_OPERATORS as readonly string[]).includes(op));
    return html`<div class="ctl-row">
      <div class="input grow">${icon('search', 14)}<input id="ctl" part="entry" type="search" .value=${draft.needle} autocomplete="off"
        @input=${(e: Event) => this.change({...draft, needle: (e.target as HTMLInputElement).value}, false)} /></div>
      <select part="mode" style="width:auto" aria-label=${`${columnCaption(this.column)} operator`} .value=${draft.op}
        @change=${(e: Event) => this.change({...draft, op: (e.target as HTMLSelectElement).value as KeywordOperator}, true)}>
        ${ops.map((op) => html`<option value=${op} ?selected=${draft.op === op}>${OPERATOR_WORDS[op]}</option>`)}
      </select>
    </div>`;
  }

  /**
   * `field`'s text with the matched span the server gave marked. The offsets count code points of
   * the served string, so the text is split by code point, not UTF-16 unit.
   */
  private markedField(v: SuggestValue, field: MatchSpan['field'], text: string): TemplateResult | string {
    if (v.match.field !== field) return text;
    const chars = [...text];
    const {start, len} = v.match;
    return html`${chars.slice(0, start).join('')}<mark>${chars.slice(start, start + len).join('')}</mark>${chars.slice(start + len).join('')}`;
  }

  /** One suggested value's text: the title, then the key muted. */
  private suggestionText(v: SuggestValue): TemplateResult {
    const title = v.title ?? v.key;
    return v.title && v.title !== v.key
      ? html`${this.markedField(v, 'title', title)}<span class="k">${this.markedField(v, 'key', v.key)}</span>`
      : html`${this.markedField(v, 'key', v.key)}`;
  }

  private category(draft: ColumnDraft & {family: 'category'}) {
    const s = this.resolvedStore;
    const suggestion = this.resolvedSuggestion;
    const refusal = this.resolvedSuggestRefusal;
    const chosen = new Set(draft.keys);
    const total = this.countedOver(suggestion);
    const toggle = (key: string) => this.change({...draft, keys: chosen.has(key) ? draft.keys.filter((k) => k !== key) : [...draft.keys, key]}, true);
    const typed = this.search !== '';
    const rows = typed ? (suggestion?.values ?? []) : [];
    // In the highlight position, a value no item passing the filter carries cannot be lit; one
    // already lit can still be taken out.
    const out = (v: SuggestValue) => this.verb === 'highlight' && v.count === 0 && !chosen.has(v.key);
    // The row the keys act on: the one the arrows reached, else the first that can be chosen.
    const choosable = rows.filter((v) => !out(v));
    const active = choosable.find((v) => v.code === this.activeCode) ?? choosable[0] ?? null;
    const move = (by: 1 | -1) => {
      if (choosable.length === 0) return;
      const at = active ? choosable.indexOf(active) : -1;
      this.activeCode = choosable[(at + by + choosable.length) % choosable.length]!.code;
    };
    const field = html`<div class="input">${icon('search', 14)}<input id="ctl" part="entry" type="search" autocomplete="off" placeholder="Type a value"
        role="combobox" aria-expanded=${rows.length > 0 ? 'true' : 'false'} aria-controls="values" aria-activedescendant=${active ? `value-${active.code}` : nothing}
        .value=${this.search}
        @input=${(e: Event) => {
          this.search = (e.target as HTMLInputElement).value;
          this.activeCode = null;
          this.ask(this.search);
        }}
        @keydown=${(e: KeyboardEvent) => {
          if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
            e.preventDefault();
            move(e.key === 'ArrowDown' ? 1 : -1);
          } else if (e.key === 'Enter') {
            if (active) toggle(active.key);
          } else if (e.key === 'Escape' && this.search) {
            e.stopPropagation();
            this.search = '';
            this.ask('');
          }
        }} /></div>`;

    const option = (v: SuggestValue) => {
      const left = out(v);
      const count = v.count;
      const share = count === undefined || total === null ? null : count === 0 ? 0 : Math.min(100, Math.max(BAR_FLOOR, (100 * count) / total));
      return html`<button type="button" part="tick" role="option" id=${`value-${v.code}`} tabindex="-1" ?data-active=${v === active}
        aria-selected=${chosen.has(v.key) ? 'true' : 'false'} aria-disabled=${left ? 'true' : 'false'}
        @click=${() => !left && toggle(v.key)}>
        <span class="opt">
          <span class="t"><span class="name">${this.suggestionText(v)}</span>${left ? html`<span class="out">none match</span>` : nothing}</span>
          ${share === null ? nothing : html`<span class="track"><span part="bar" style=${`display:block;width:${share.toFixed(1)}%`}></span></span>`}
        </span>
        ${count === undefined ? nothing : html`<span part="value-count">${count.toLocaleString('en-GB')}</span>`}
      </button>`;
    };
    const list =
      rows.length > 0
        ? html`<div part="values" id="values" role="listbox" aria-label=${`${columnCaption(this.column)} values`}>${repeat(rows, (v) => v.code, option)}</div>`
        : nothing;
    const top = this.topValues();
    const topKeys = new Set(top?.values.map((v) => v.key) ?? []);
    const topList =
      top && top.values.length > 0
        ? html`<div part="top" role="group" aria-label=${`Commonest ${columnCaption(this.column)} values`}>
            ${top.values.map((v) => {
              const title = v.title ?? keyTitle(s, this.column, v.key);
              const on = chosen.has(v.key);
              const share = top.total > 0 ? (v.count === 0 ? 0 : Math.min(100, Math.max(BAR_FLOOR, (100 * v.count) / top.total))) : 0;
              return html`<label part="top-value" data-key=${v.key}>
                <input type="checkbox" .checked=${on} @change=${() => toggle(v.key)} />
                <span class="opt"><span class="name" title=${v.key}>${title}</span><span class="track"><span part="bar" style=${`display:block;width:${share.toFixed(1)}%`}></span></span></span>
                <span part="value-count">${v.count.toLocaleString('en-GB')}</span>
              </label>`;
            })}
          </div>`
        : nothing;
    const note = !typed
      ? nothing
      : refusal
        ? html`<span part="refusal" data-code=${refusal.code}><span class="dot refuse"></span>Values unavailable</span>`
        : suggestion === null
          ? html`<span class="skel" aria-hidden="true"></span>`
          : rows.length === 0
            ? html`<span part="more">No value starts with that</span>`
            : suggestion.more
              ? html`<span part="more">Type more to narrow the list</span>`
              : nothing;
    const rest = draft.keys.filter((key) => !topKeys.has(key));
    const chips =
      rest.length > 0
        ? html`<div class="chosen">
            ${rest.map((key) => {
              const title = keyTitle(s, this.column, key);
              return html`<span part="chosen" class="chip" data-verb=${this.verb}>${title}<button type="button" aria-label=${`Remove ${title} from ${columnCaption(this.column)}`} @click=${() => toggle(key)}>${icon('close', 12)}</button></span>`;
            })}
          </div>`
        : nothing;
    return html`<div class="combo">${field}${list}</div>${note}${topList}${chips}`;
  }

  private numeric(draft: {family: 'numeric'; gte: number | null; lte: number | null}) {
    const column = this.resolvedStore?.get('meta')?.declaredScalars.find((c) => c.name === this.column);
    const date = column?.arrowType === 'timestamp_us';
    const caption = columnCaption(this.column);
    const bound = (which: 'gte' | 'lte') => {
      const held = draft[which];
      const invalid = this.invalid[which];
      const shown = invalid ?? (held === null ? '' : date ? shortDateText(held) : String(held));
      const read = (raw: string): number | null | undefined => {
        if (raw.trim() === '') return null;
        const n = date ? parseDateText(raw, which === 'lte') : Number(raw);
        return n !== null && Number.isFinite(n) ? n : undefined;
      };
      const commit = (e: Event) => {
        const raw = (e.target as HTMLInputElement).value;
        const value = read(raw);
        if (value === undefined) {
          this.invalid = {...this.invalid, [which]: raw};
          return;
        }
        const {[which]: _dropped, ...rest} = this.invalid;
        this.invalid = rest;
        if (value !== held) this.change({...draft, [which]: value} as ColumnDraft, true);
        else this.requestUpdate();
      };
      return html`<input id=${which === 'gte' ? 'ctl' : nothing} part="entry" type=${date ? 'text' : 'number'} inputmode=${date ? nothing : 'decimal'}
        placeholder=${date ? (which === 'gte' ? 'Earliest' : 'Latest') : which === 'gte' ? 'Lowest' : 'Highest'} .value=${shown}
        aria-label=${`${caption} ${which === 'gte' ? 'from' : 'to'}`} aria-invalid=${invalid !== undefined ? 'true' : 'false'}
        @change=${commit} @keydown=${(e: KeyboardEvent) => e.key === 'Enter' && commit(e)} />`;
    };
    return html`<div class="range">${bound('gte')}<span class="to">to</span>${bound('lte')}</div>`;
  }
}

attachContextRoot();
defineOnce('tessera-filter', TesseraFilter);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-filter': TesseraFilter;
  }
}
