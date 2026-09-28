import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {composeFilters, emptyDraft, isPopulated, type ClauseVerb, type ColumnDraft, type FilterOperandSet, type MatchSpan, type Refusal, type SuggestValue} from '@tesseradb/client';
import {TesseraElement, columnCaption, emit, keyTitle, parseDateText, shortDateText} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {chrome, tokens} from './tokens.js';

/** The operators a keyword control can send. */
const KEYWORD_OPERATORS = ['contains', 'prefix', 'eq'] as const;
type KeywordOperator = (typeof KEYWORD_OPERATORS)[number];
/** Each operator as the select says it. */
const OPERATOR_WORDS: Record<KeywordOperator, string> = {contains: 'contains', prefix: 'starts with', eq: 'is'};

/** How long a typed control must be quiet before its change is sent. */
const TYPING_DEBOUNCE_MS = 350;

/** The narrowest bar a suggested value with any items draws, as a percentage, so it still shows. */
const BAR_FLOOR = 2;

/**
 * One filter control for the column `column`, drawn by the family `/v1/meta` gives the column, under
 * the column's name. `verb` is the position it edits: the column's filter clause or its highlight
 * clause. The two are separate clauses; the control shows the one in its position and leaves the
 * other alone.
 *
 * A text column is one search box. Its words must all appear; words in double quotes are a phrase;
 * `OR` between terms asks for either. A line under the box says so, leaving out the phrase where
 * the column takes none.
 *
 * A category is a search box over `/v1/categories/{column}/suggest`, which lists nothing until
 * something is typed. Each value suggested shows its count in the current view and a bar, its
 * share of the items the map matches now. In the highlight position a value the column's filter
 * leaves out is greyed, counted 0 and cannot be chosen. The values chosen sit under the box as
 * chips, each with a ×. A value typed and entered is added whether or not it was suggested; a key
 * the viewer cannot see matches nothing, as a key that does not exist does. Where the legend holds
 * the column's values, the heading says how many there are.
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
 * @csspart tick - One suggested value, with `aria-selected`, and `aria-disabled` where the filter
 *   leaves it out.
 * @csspart bar - A suggested value's share of the items matched.
 * @csspart value-count - A suggested value's count.
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
      [part='values'] {
        display: flex;
        flex-direction: column;
        margin-top: 6px;
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
      [part~='tick']:focus-visible {
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
  /** The date inputs whose text did not read as a date, which keep the text typed. @internal */
  @state() accessor invalid: {gte?: string; lte?: string} = {};
  private sent: ColumnDraft | null = null;
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

  /** The suggestion page for `this.search`, or `null` while it is stale or has not landed. */
  private get resolvedSuggestion(): {q: string; values: SuggestValue[]; more: boolean} | null {
    const s = this.resolvedStore?.get('filters').suggestions[this.column];
    return s && s.q === this.search ? s : null;
  }

  private get resolvedSuggestRefusal(): Refusal | null {
    return this.resolvedStore?.get('filters').suggestErrors[this.column] ?? null;
  }

  /** Ask the store's typeahead for `q`, once per distinct `q`; nothing is asked before anything is typed. */
  private ask(q: string): void {
    const s = this.resolvedStore;
    if (!s || q === '' || this.lastAsked === q) return;
    this.lastAsked = q;
    s.suggest(this.column, q);
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
      const values = this.resolvedStore?.get('legend').categories[this.column];
      return values ? html`<span part="aside">${values.length.toLocaleString('en-GB')} ${values.length === 1 ? 'value' : 'values'}</span>` : nothing;
    }
    if (draft.family === 'numeric' && isPopulated(draft)) {
      return html`<button part="aside" type="button" @click=${() => {
        this.invalid = {};
        this.change({family: 'numeric', gte: null, lte: null}, true);
      }}>Clear</button>`;
    }
    return nothing;
  }

  private body(o: FilterOperandSet, draft: ColumnDraft) {
    switch (draft.family) {
      case 'text':
        return this.text(o, draft);
      case 'keyword':
        return this.keyword(o, draft);
      case 'category':
        return this.category(draft);
      case 'numeric':
        return this.numeric(draft);
    }
  }

  private text(o: FilterOperandSet, draft: ColumnDraft & {family: 'text'}) {
    const hint = o.operands.includes('phrase') ? 'Words match together. Use “quotes” for a phrase, OR for either.' : 'Words match together. Use OR for either.';
    return html`<div class="input">${icon('search', 14)}<input id="ctl" part="entry" type="search" .value=${draft.query} placeholder="Search the text" autocomplete="off"
        aria-describedby="hint"
        @input=${(e: Event) => this.change({family: 'text', query: (e.target as HTMLInputElement).value}, false)} /></div>
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
    // In the highlight position, the values the column's own filter leaves out cannot be lit.
    const filtered = this.verb === 'highlight' ? s?.get('filters').draft.filter[this.column] : undefined;
    const kept = filtered?.family === 'category' && filtered.keys.length > 0 ? new Set(filtered.keys) : null;
    const matched = s?.get('view').matched;
    const total = matched && matched.value > 0 ? matched.value : null;
    const toggle = (key: string) => this.change({...draft, keys: chosen.has(key) ? draft.keys.filter((k) => k !== key) : [...draft.keys, key]}, true);

    // Enter submits what is typed, suggested or not; see the element's doc.
    const submit = () => {
      const key = this.search.trim();
      if (!key || chosen.has(key)) return;
      this.change({...draft, keys: [...draft.keys, key]}, true);
    };
    const field = html`<div class="input">${icon('search', 14)}<input id="ctl" part="entry" type="search" autocomplete="off" placeholder="Type a value"
        .value=${this.search}
        @input=${(e: Event) => {
          this.search = (e.target as HTMLInputElement).value;
          this.ask(this.search);
        }}
        @keydown=${(e: KeyboardEvent) => {
          if (e.key === 'Enter') submit();
          if (e.key === 'Escape' && this.search) {
            e.stopPropagation();
            this.search = '';
          }
        }} /></div>`;

    const option = (v: SuggestValue) => {
      const out = kept !== null && !kept.has(v.key);
      const count = out ? 0 : v.count;
      const share = count === undefined || total === null ? null : count === 0 ? 0 : Math.min(100, Math.max(BAR_FLOOR, (100 * count) / total));
      return html`<button type="button" part="tick" role="option" aria-selected=${chosen.has(v.key) ? 'true' : 'false'} aria-disabled=${out ? 'true' : 'false'}
        @click=${() => !out && toggle(v.key)}>
        <span class="opt">
          <span class="t"><span class="name">${this.suggestionText(v)}</span>${out ? html`<span class="out">filtered out</span>` : nothing}</span>
          ${share === null ? nothing : html`<span class="track"><span part="bar" style=${`display:block;width:${share.toFixed(1)}%`}></span></span>`}
        </span>
        ${count === undefined ? nothing : html`<span part="value-count">${count.toLocaleString('en-GB')}</span>`}
      </button>`;
    };
    const typed = this.search !== '';
    const rows = suggestion?.values ?? [];
    const list =
      typed && rows.length > 0
        ? html`<div part="values" role="listbox" aria-label=${`${columnCaption(this.column)} values`}>${repeat(rows, (v) => v.code, option)}</div>`
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
    const chips =
      draft.keys.length > 0
        ? html`<div class="chosen">
            ${draft.keys.map((key) => {
              const title = keyTitle(s, this.column, key);
              return html`<span part="chosen" class="chip" data-verb=${this.verb}>${title}<button type="button" aria-label=${`Remove ${title}`} @click=${() => toggle(key)}>${icon('close', 12)}</button></span>`;
            })}
          </div>`
        : nothing;
    return html`${field}${list}${note}${chips}`;
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
