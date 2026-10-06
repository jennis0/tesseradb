import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {composeFilters, emptyDraft, type ColumnDraft, type FilterOperandSet, type MatchSpan, type Refusal, type SuggestionPage, type SuggestValue} from '@tesseradb/client';
import {FloatingList} from './float.js';
import {TesseraElement, columnCaption, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {chrome, tokens} from './tokens.js';

/** The operators a keyword control can send. */
const KEYWORD_OPERATORS = ['contains', 'prefix', 'eq'] as const;
type KeywordOperator = (typeof KEYWORD_OPERATORS)[number];
/** Each operator as the menu and a chip say it. */
export const OPERATOR_WORDS: Record<KeywordOperator, string> = {contains: 'contains', prefix: 'starts with', eq: 'is'};

/** How long a typed control must be quiet before its change is sent. */
const TYPING_DEBOUNCE_MS = 350;

/** The narrowest bar a suggested value with any items draws, as a percentage, so it still shows. */
const BAR_FLOOR = 2;

/**
 * The search box of a field card for the column `column`, drawn by the family `/v1/meta` gives the
 * column. It edits the column's clause in the filter position (`Store.setFilters`).
 *
 * A text column is one box. Its words must all appear; words in double quotes are a phrase, or
 * plain words where the column takes no phrase; `OR` between terms asks for either. A text clause
 * set from outside that no query writes shows read-only.
 *
 * A category is a box over `/v1/categories/{column}/suggest`. Its suggestions open over what sits
 * below the box while the box has focus and holds text. Each value suggested shows its count in
 * the current view among the items passing the filter without the column's own clause, so a value
 * counts what choosing it as well would add, and a bar, its share of the total the server counted
 * over. The arrow keys move through the suggestions, and Enter chooses the one reached, the first
 * by default, or takes it out where it is chosen; text that suggests nothing chooses nothing.
 * Emptying the box, or removing the control, has the store stop asking for the column.
 *
 * A keyword column is a box with its operator (`contains`, `prefix` or `eq`) in a menu.
 *
 * Typing is sent 350 ms after the last keystroke; a choice is sent at once. Each change replaces
 * the column's control in the filter position of the store's draft.
 *
 * @summary A field card's search box.
 * @tagname tessera-filter
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - The box
 *   changed the column's filter, with the expression the filter position composes.
 * @csspart entry - The box.
 * @csspart mode - The keyword column's operator button, which opens its menu.
 * @csspart operators - The menu of a keyword column's operators.
 * @csspart operator - One operator in the menu, with `aria-checked`.
 * @csspart values - The typeahead's suggestions.
 * @csspart tick - One suggested value, with `aria-selected`.
 * @csspart bar - A suggested value's share of the items its count is taken over.
 * @csspart value-count - A suggested value's count.
 * @csspart more - The note under the box: nothing starts with the text, or more values match than
 *   one page holds.
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
      .input {
        height: 28px;
        gap: 6px;
        padding: 0 8px;
        font-size: 12px;
      }
      .input input {
        font-size: 12px;
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
      /* The operator: a button that opens a menu, as the explorer's choices do. */
      .op {
        position: relative;
        flex: none;
      }
      [part='mode'] {
        display: flex;
        align-items: center;
        gap: 6px;
        height: 28px;
        padding: 0 8px 0 10px;
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius-control);
        background: var(--_tessera-surface);
        font-size: 12px;
        color: var(--_tessera-ink);
        white-space: nowrap;
      }
      [part='operators'] {
        position: fixed;
        inset: auto;
        margin: 0;
        box-sizing: border-box;
        min-width: 120px;
        padding: 4px;
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius-control);
        background: var(--_tessera-surface);
        color: var(--_tessera-ink);
        box-shadow: 0 6px 18px rgba(0, 0, 0, 0.08);
      }
      [part~='operator'] {
        display: block;
        width: 100%;
        padding: 6px 8px;
        border-radius: 4px;
        font-size: 13px;
        text-align: left;
        white-space: nowrap;
      }
      [part~='operator']:hover,
      [part~='operator']:focus-visible,
      [part~='operator'][aria-checked='true'] {
        background: var(--_tessera-surface-2);
      }
      [part~='operator'][aria-checked='true'] {
        font-weight: 500;
      }
      .combo {
        position: relative;
      }
      /* The suggestions open over what sits below the box, in the top layer. */
      [part='values'] {
        position: fixed;
        inset: auto;
        margin: 0;
        box-sizing: border-box;
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
      [part~='tick'][data-active],
      [part~='tick'][aria-selected='true'] {
        background: var(--_tessera-surface-2);
      }
      [part~='tick'][aria-selected='true'] {
        font-weight: 600;
      }
      .opt {
        display: flex;
        flex-direction: column;
        gap: 3px;
        min-width: 0;
      }
      .opt .name {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
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
      /* Every count in a list as wide as the widest, so the bars' tracks end together. */
      [part='value-count'] {
        min-width: var(--_count-width, auto);
        text-align: right;
        font-size: 12px;
        font-weight: 400;
        font-variant-numeric: tabular-nums;
        color: var(--_tessera-ink-2);
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
    `
  ];

  /** The column this box filters, a name `meta.filterOperands` lists. Unset or unknown, it renders nothing. */
  @property() accessor column = '';
  /** The box's placeholder, in place of its own. */
  @property() accessor placeholder = '';

  /** @internal */
  @state() accessor draft: ColumnDraft | null = null;
  /** @internal */
  @state() accessor search = '';
  /** The suggestion the arrow keys moved to, by code; `null` is the first. @internal */
  @state() accessor activeCode: number | null = null;
  /** Whether the category box has focus, which its suggestions show only while it does. @internal */
  @state() accessor focused = false;
  /** Whether a keyword column's operator menu is open. @internal */
  @state() accessor operatorsOpen = false;
  private sent: ColumnDraft | null = null;
  private readonly floating = new FloatingList(() => {
    const list = this.renderRoot.querySelector<HTMLElement>('[part="values"]');
    const anchor = this.renderRoot.querySelector<HTMLElement>('.combo');
    return list && anchor ? {list, anchor} : null;
  });
  private readonly operatorMenu = new FloatingList(() => {
    const list = this.renderRoot.querySelector<HTMLElement>('[part="operators"]');
    const anchor = this.renderRoot.querySelector<HTMLElement>('.op');
    return list && anchor ? {list, anchor} : null;
  });
  private typing: ReturnType<typeof setTimeout> | null = null;
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
    return this.resolvedStore?.get('meta')?.filterOperands.find((o) => o.column === this.column) ?? null;
  }

  /** The suggestion page for `this.search`, or `null` while it is stale or has not landed. */
  private get resolvedSuggestion(): SuggestionPage | null {
    const s = this.resolvedStore?.get('filters').suggestions[this.column];
    return s && s.q === this.search && s.verb === 'filter' ? s : null;
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
    s.suggest(this.column, q, 'filter');
  }

  override disconnectedCallback(): void {
    this.ask('');
    this.floating.stop();
    this.operatorMenu.stop();
    super.disconnectedCallback();
  }

  protected override onStoreChange(): void {
    // Re-seed from the store only when its draft changed underneath (a clear all, or the first
    // meta), not while the user's own edit is in flight.
    const stored = this.resolvedStore?.get('filters').draft.filter[this.column] ?? null;
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

  /** Moves focus to the box. */
  override focus(options?: FocusOptions): void {
    const target = this.renderRoot.querySelector<HTMLElement>('#ctl');
    if (target) target.focus(options);
    else super.focus(options);
  }

  protected override updated(changed: PropertyValues<this>): void {
    super.updated(changed);
    this.floating.update();
    this.operatorMenu.update();
  }

  /** The box's draft: the one being edited, else the store's, else the client's empty one, which also sets a keyword's starting operator. */
  private currentDraft(o: FilterOperandSet): ColumnDraft | null {
    return this.draft ?? this.resolvedStore?.get('filters').draft.filter[this.column] ?? emptyDraft([o]).filter[o.column] ?? null;
  }

  private change(next: ColumnDraft, immediate: boolean): void {
    this.draft = next;
    if (this.typing) clearTimeout(this.typing);
    const column = this.column;
    const apply = () => {
      this.typing = null;
      const s = this.resolvedStore;
      if (!s) return;
      const held = s.get('filters').draft;
      const draft = {...held, filter: {...held.filter, [column]: next}};
      this.sent = next;
      s.setFilters(draft);
      emit(this, 'tessera-filterchange', {column, verb: 'filter', expr: composeFilters(draft, 'filter')});
    };
    if (immediate) apply();
    else this.typing = setTimeout(apply, TYPING_DEBOUNCE_MS);
  }

  override render(): TemplateResult | typeof nothing {
    const o = this.resolvedOperand;
    if (!o) return nothing;
    const draft = this.currentDraft(o);
    if (!draft) return nothing;
    switch (draft.family) {
      case 'text':
        return this.text(draft);
      case 'keyword':
        return this.keyword(o, draft);
      case 'category':
        return this.category(draft);
      case 'numeric':
        return nothing;
    }
  }

  private label(): string {
    return this.placeholder || columnCaption(this.column);
  }

  private text(draft: ColumnDraft & {family: 'text'}) {
    // An expression set from outside that no query writes is shown as it is, and cannot be typed over.
    if (draft.expr !== undefined) {
      return html`<div class="input">${icon('search', 12)}<input id="ctl" part="entry" readonly aria-label=${this.label()} .value=${JSON.stringify(draft.expr)} /></div>`;
    }
    return html`<div class="input">${icon('search', 12)}<input id="ctl" part="entry" type="search" .value=${draft.query} placeholder=${this.placeholder || 'Search the text'} autocomplete="off"
        aria-label=${this.label()} @input=${(e: Event) => this.change({family: 'text', query: (e.target as HTMLInputElement).value, phrase: draft.phrase}, false)} /></div>`;
  }

  /** A keyword box, offering the operators the column publishes in a menu. */
  private keyword(o: FilterOperandSet, draft: ColumnDraft & {family: 'keyword'}) {
    const ops = o.operands.filter((op): op is KeywordOperator => (KEYWORD_OPERATORS as readonly string[]).includes(op));
    const caption = columnCaption(this.column);
    const close = (refocus: boolean) => {
      this.operatorsOpen = false;
      if (refocus) void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="mode"]')?.focus());
    };
    const choose = (op: KeywordOperator) => {
      close(true);
      if (op !== draft.op) this.change({...draft, op}, true);
    };
    const at = Math.max(0, ops.indexOf(draft.op));
    const keys = (e: KeyboardEvent, i: number) => {
      const items = Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="operator"]'));
      if (e.key === 'Escape') {
        e.stopPropagation();
        close(true);
        return;
      }
      const next = {ArrowDown: (i + 1) % items.length, ArrowUp: (i - 1 + items.length) % items.length, Home: 0, End: items.length - 1}[e.key];
      if (next === undefined) return;
      e.preventDefault();
      items[next]?.focus();
    };
    const open = () => {
      this.operatorsOpen = true;
      void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part~="operator"][aria-checked="true"]')?.focus());
    };
    return html`<div class="ctl-row">
      <div class="input grow">${icon('search', 12)}<input id="ctl" part="entry" type="search" .value=${draft.needle} autocomplete="off" placeholder=${this.placeholder || nothing} aria-label=${this.label()}
        @input=${(e: Event) => this.change({...draft, needle: (e.target as HTMLInputElement).value}, false)} /></div>
      <div class="op" @focusout=${(e: FocusEvent) => {
        const to = e.relatedTarget as Node | null;
        if (this.operatorsOpen && !(to && (e.currentTarget as HTMLElement).contains(to))) this.operatorsOpen = false;
      }}>
        <button part="mode" type="button" aria-haspopup="menu" aria-expanded=${this.operatorsOpen ? 'true' : 'false'} aria-label=${`${caption} operator: ${OPERATOR_WORDS[draft.op]}`}
          @click=${() => (this.operatorsOpen ? close(false) : open())}
          @keydown=${(e: KeyboardEvent) => {
            if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
            e.preventDefault();
            open();
          }}>${OPERATOR_WORDS[draft.op]}${icon('chev', 12, 1.4)}</button>
        ${this.operatorsOpen
          ? html`<div part="operators" popover="manual" role="menu" aria-label=${`${caption} operator`}>
              ${ops.map(
                (op, i) => html`<button part="operator" type="button" role="menuitemradio" data-op=${op} aria-checked=${op === draft.op ? 'true' : 'false'} tabindex=${i === at ? '0' : '-1'}
                  @click=${() => choose(op)} @keydown=${(e: KeyboardEvent) => keys(e, i)}>${OPERATOR_WORDS[op]}</button>`
              )}
            </div>`
          : nothing}
      </div>
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
    const suggestion = this.resolvedSuggestion;
    const refusal: Refusal | null = this.resolvedStore?.get('filters').suggestErrors[this.column] ?? null;
    const chosen = new Set(draft.keys);
    const total = suggestion?.total ? suggestion.total : null;
    const toggle = (key: string) => this.change({...draft, keys: chosen.has(key) ? draft.keys.filter((k) => k !== key) : [...draft.keys, key]}, true);
    const typed = this.search !== '';
    // The suggestions show while the box has focus and holds text; the list closes as focus leaves.
    const rows = typed && this.focused ? (suggestion?.values ?? []) : [];
    // The row the keys act on: the one the arrows reached, else the first.
    const active = rows.find((v) => v.code === this.activeCode) ?? rows[0] ?? null;
    const move = (by: 1 | -1) => {
      if (rows.length === 0) return;
      const at = active ? rows.indexOf(active) : -1;
      this.activeCode = rows[(at + by + rows.length) % rows.length]!.code;
    };
    const field = html`<div class="input">${icon('search', 12)}<input id="ctl" part="entry" type="search" autocomplete="off" placeholder=${this.placeholder || 'Type a value'}
        aria-label=${this.label()} role="combobox" aria-expanded=${rows.length > 0 ? 'true' : 'false'} aria-controls="values" aria-activedescendant=${active ? `value-${active.code}` : nothing}
        .value=${this.search}
        @focus=${() => (this.focused = true)}
        @blur=${() => (this.focused = false)}
        @input=${(e: Event) => {
          this.focused = true;
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
      const count = v.count;
      const share = count === undefined || total === null ? null : count === 0 ? 0 : Math.min(100, Math.max(BAR_FLOOR, (100 * count) / total));
      return html`<button type="button" part="tick" role="option" id=${`value-${v.code}`} tabindex="-1" ?data-active=${v === active}
        aria-selected=${chosen.has(v.key) ? 'true' : 'false'} @mousedown=${(e: Event) => e.preventDefault()} @click=${() => toggle(v.key)}>
        <span class="opt">
          <span class="name">${this.suggestionText(v)}</span>
          ${share === null ? nothing : html`<span class="track"><span part="bar" style=${`display:block;width:${share.toFixed(1)}%`}></span></span>`}
        </span>
        ${count === undefined ? nothing : html`<span part="value-count">${count.toLocaleString('en-GB')}</span>`}
      </button>`;
    };
    const list =
      rows.length > 0
        ? html`<div part="values" id="values" popover="manual" role="listbox" aria-label=${`${columnCaption(this.column)} values`} style=${countWidth(rows.map((v) => v.count))}>${repeat(rows, (v) => v.code, option)}</div>`
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
    return html`<div class="combo">${field}${list}</div>${note}`;
  }
}

/** The width of the widest of `counts` as written, for `--_count-width`. */
function countWidth(counts: readonly (number | undefined)[]): string {
  const widest = Math.max(0, ...counts.map((n) => (n === undefined ? 0 : n.toLocaleString('en-GB').length)));
  return widest > 0 ? `--_count-width:${widest}ch` : '';
}

attachContextRoot();
defineOnce('tessera-filter', TesseraFilter);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-filter': TesseraFilter;
  }
}
