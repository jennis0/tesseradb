import {css, html, nothing, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {
  composeFilters,
  emptyDraft,
  isPopulated,
  type ColumnDraft,
  type FilterOperandSet,
  type MatchSpan,
  type Refusal,
  type SuggestValue,
  type TextMode
} from '@tesseradb/client';
import {TesseraElement, columnCaption, emit} from './base.js';
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

/**
 * One filter control for the column `column`, drawn by the family `/v1/meta` gives the column. A
 * text column is a search box and two buttons: all words, and phrase (any word where the column
 * takes no phrase). A category is a checklist when every value the viewer can see fits on the first page of suggestions, else a
 * typeahead over `/v1/categories/{column}/suggest`. A number is two inputs, and a date two date
 * inputs. A keyword column is a text box with its operator (`contains`, `prefix` or `eq`).
 *
 * Typing is sent 350 ms after the last keystroke; a choice is sent at once. Each change replaces
 * the column's control in the store's filter draft (`Store.setFilters`). A category value typed
 * and entered is added whether or not it was suggested; a key the viewer cannot see matches
 * nothing, as a key that does not exist does. The host carries `data-on` while the control holds a
 * value.
 *
 * @summary One filter control, drawn by the column's type.
 * @tagname tessera-filter
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - The
 *   control changed, with the column and the composed filter expression.
 * @csspart label - The column's name, as a caption.
 * @csspart entry - A text, number or date input.
 * @csspart mode - The text column's word toggle, or the keyword column's operator select.
 * @csspart values - The checklist, or the typeahead's suggestions.
 * @csspart tick - One value in the checklist or the suggestions.
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
      [part='label'] {
        display: block;
        margin-bottom: 8px;
        font-weight: 600;
        color: var(--_tessera-ink);
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
      .seg {
        margin-top: 6px;
      }
      [part='values'] {
        margin-top: 4px;
        display: flex;
        flex-direction: column;
        gap: 2px;
      }
      /* A row is a button stretched to the list's width; the shared button reset leaves text-align. */
      [part='tick'] {
        text-align: left;
      }
      [part='tick'] .t {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      /* The matched span the server gave. */
      [part='tick'] mark {
        background: none;
        color: inherit;
        font-weight: 600;
      }
      [part='tick'] input {
        width: 14px;
        height: 14px;
        margin: 0;
        flex: none;
      }
      [part='values'][role='group'] > [part='tick'] {
        min-height: 28px;
        padding-left: 2px;
      }
      /* The key follows the title, muted, since a key may be an opaque identifier such as a uuid. */
      [part='tick'] .k {
        margin-left: 0.45em;
        opacity: 0.55;
        font-size: 0.85em;
        font-variant-numeric: tabular-nums;
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
      .to {
        color: var(--_tessera-ink-3);
      }
      input.num {
        font-variant-numeric: tabular-nums;
      }
    `
  ];

  /** The column this control filters, a name `meta.filterOperands` lists. Unset or unknown, the control renders nothing. */
  @property() accessor column = '';
  /** The column's operands, for a host that sets them itself in place of the store's `meta`. */
  @property({attribute: false}) accessor operand: FilterOperandSet | null = null;

  /** @internal */
  @state() accessor draft: ColumnDraft | null = null;
  /** @internal */
  @state() accessor search = '';
  /**
   * Which category shape this control draws: `null` until the empty-`q` page answers, then
   * `'checklist'` if it said `more: false` (every visible value fits) or `'lookahead'` if not.
   * Decided once from that page, so later pages do not flip it, and reset when the store's
   * `suggestEpoch` moves.
   *
   * @internal
   */
  @state() accessor shape: 'checklist' | 'lookahead' | null = null;
  private sent: ColumnDraft | null = null;
  private typing: ReturnType<typeof setTimeout> | null = null;
  /**
   * The last `q` this element asked the store's typeahead for. Store changes arrive for every
   * projection, and re-asking on each would keep re-arming the store's debounce so no request went
   * out; asking only on a new `q` avoids that.
   */
  private lastAsked: string | null = null;
  /**
   * The store's `filters.suggestEpoch` as last seen, `-1` before the first update. A change means
   * every held page was invalidated (a view switch or re-authorisation). A column still loading or
   * refused looks the same before and after a reset, so only the epoch shows one happened.
   */
  private lastEpoch = -1;

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

  /** Ask the store's typeahead for `q`, but only once per distinct `q` this element has asked. */
  private ask(q: string): void {
    const s = this.resolvedStore;
    if (!s || this.lastAsked === q) return;
    this.lastAsked = q;
    s.suggest(this.column, q);
  }

  protected override onStoreChange(): void {
    // Re-seed from the store only when its draft changed underneath (a clear all, or the first
    // meta), not while the user's own edit is in flight.
    const stored = this.resolvedStore?.get('filters').draft[this.column] ?? null;
    if (stored && stored !== this.sent && JSON.stringify(stored) !== JSON.stringify(this.draft)) {
      this.draft = structuredClone(stored);
      this.sent = stored;
    }
    if (this.resolvedOperand?.family === 'category') {
      const filters = this.resolvedStore?.get('filters');
      const epoch = filters?.suggestEpoch ?? -1;
      // Invalidated: forget the shape, the typed `q` and `lastAsked`, so the `ask('')` below
      // reaches the store and the control starts over as on first mount.
      if (epoch !== this.lastEpoch) {
        this.lastEpoch = epoch;
        this.shape = null;
        this.lastAsked = null;
        this.search = '';
      }
      // Not an `else`: the first update after mount takes the branch above, and a page may
      // already be on the store then.
      const page = filters?.suggestions[this.column];
      if (page && page.q === '' && this.shape === null) this.shape = page.more ? 'lookahead' : 'checklist';
      // An empty `q` lists values before anything is typed. `ask` reaches the store once per `q`.
      this.ask(this.search);
    }
    super.onStoreChange();
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
    return this.draft ?? this.resolvedStore?.get('filters').draft[this.column] ?? emptyDraft([o])[o.column] ?? null;
  }

  private change(next: ColumnDraft, immediate: boolean): void {
    this.draft = next;
    if (this.typing) clearTimeout(this.typing);
    const apply = () => {
      this.typing = null;
      const s = this.resolvedStore;
      if (!s) return;
      const draft = {...s.get('filters').draft, [this.column]: next};
      this.sent = next;
      s.setFilters(draft);
      emit(this, 'tessera-filterchange', {column: this.column, expr: composeFilters(draft)});
    };
    if (immediate) apply();
    else this.typing = setTimeout(apply, TYPING_DEBOUNCE_MS);
  }

  override render(): TemplateResult | typeof nothing {
    const o = this.resolvedOperand;
    if (!o) return nothing;
    const draft = this.currentDraft(o);
    if (!draft) return nothing;
    // The checklist is a `role="group"`, which `for` cannot label, so it uses `aria-labelledby`;
    // every other shape has an `input` with `id="ctl"`.
    const checklist = draft.family === 'category' && this.shape === 'checklist';
    const label = checklist
      ? html`<span part="label" class="muted" id="ctl-label">${columnCaption(this.column)}</span>`
      : html`<label part="label" class="muted" for="ctl">${columnCaption(this.column)}</label>`;
    return html`${label}${this.body(o, draft)}`;
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
    const modes: [TextMode, string][] = [['all', 'all words']];
    if (o.operands.includes('phrase')) modes.push(['phrase', 'phrase']);
    else modes.push(['any', 'any word']);
    return html`<div class="input">${icon('search', 14)}<input id="ctl" part="entry" type="search" .value=${draft.query} placeholder="" autocomplete="off"
        aria-label=${`${o.column} words`}
        @input=${(e: Event) => this.change({...draft, query: (e.target as HTMLInputElement).value}, false)} /></div>
      <div class="seg" part="mode" role="group" aria-label=${`${o.column} mode`}>
        ${modes.map(([v, t]) => html`<button type="button" data-mode=${v} aria-pressed=${draft.mode === v ? 'true' : 'false'} @click=${() => this.change({...draft, mode: v}, true)}>${t}</button>`)}
      </div>`;
  }

  /** A keyword control, offering the operators the column publishes. */
  private keyword(o: FilterOperandSet, draft: ColumnDraft & {family: 'keyword'}) {
    const ops = o.operands.filter((op): op is KeywordOperator => (KEYWORD_OPERATORS as readonly string[]).includes(op));
    return html`<div class="ctl-row">
      <div class="input grow">${icon('search', 14)}<input id="ctl" part="entry" type="search" .value=${draft.needle} autocomplete="off"
        aria-label=${`${this.column} value`}
        @input=${(e: Event) => this.change({...draft, needle: (e.target as HTMLInputElement).value}, false)} /></div>
      <select part="mode" style="width:auto" aria-label=${`${this.column} operator`} .value=${draft.op}
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

  /** A checklist row's text: the title where there is one, else the key. The key is its tooltip. */
  private checklistText(v: SuggestValue): string {
    return v.title ?? v.key;
  }

  private category(draft: ColumnDraft & {family: 'category'}) {
    const suggestion = this.resolvedSuggestion;
    const refusal = this.resolvedSuggestRefusal;
    const chosen = new Set(draft.keys);

    const pick = (v: SuggestValue) => {
      this.change(chosen.has(v.key) ? {...draft, keys: draft.keys.filter((k) => k !== v.key)} : {...draft, keys: [...draft.keys, v.key]}, true);
    };
    // The checklist: every visible value is on the page, so a checkbox per value and no search
    // box. It uses the same `pick` as the lookahead, so the draft sent is the same. The chosen
    // values show as ticks here and as the filter panel's chip.
    if (this.shape === 'checklist') {
      const rows = suggestion?.values ?? [];
      // A checklist does not re-ask, so a refusal here is unexpected; it is shown if it happens.
      const refusalNote = refusal ? html`<span part="refusal" data-code=${refusal.code}><span class="dot refuse"></span>Values unavailable</span>` : nothing;
      return html`<div part="values" class="list" role="group" aria-labelledby="ctl-label">
        ${repeat(
          rows,
          (v) => v.code,
          (v) => html`<label part="tick" class="item" title=${v.key}>
              <input type="checkbox" .checked=${chosen.has(v.key)} @change=${() => pick(v)} aria-label=${v.title ?? v.key} />
              <span class="t">${this.checklistText(v)}</span>
            </label>`
        )}
      </div>${refusalNote}`;
    }

    // Enter submits what is typed, suggested or not; see the element's doc.
    const submit = () => {
      const key = this.search.trim();
      if (!key || chosen.has(key)) return;
      this.search = '';
      this.ask('');
      this.change({...draft, keys: [...draft.keys, key]}, true);
    };
    const field = html`<div class="input">${icon('search', 14)}<input id="ctl" part="entry" type="search" autocomplete="off"
        aria-label=${`${this.column} value`} .value=${this.search}
        @input=${(e: Event) => {
          this.search = (e.target as HTMLInputElement).value;
          this.ask(this.search);
        }}
        @keydown=${(e: KeyboardEvent) => {
          if (e.key === 'Enter') submit();
        }} /></div>`;

    const rows = suggestion?.values ?? [];
    const list =
      rows.length > 0
        ? html`<div part="values" class="list" role="listbox" aria-label=${`${this.column} suggestions`}>
            ${repeat(
              rows,
              (v) => v.code,
              (v) => html`<button type="button" part="tick" class="item" role="option" aria-selected=${chosen.has(v.key) ? 'true' : 'false'} @click=${() => pick(v)}>
                  <span class="t">${this.suggestionText(v)}</span>
                </button>`
            )}
          </div>`
        : nothing;

    const note = refusal
      ? html`<span part="refusal" data-code=${refusal.code}><span class="dot refuse"></span>Values unavailable</span>`
      : suggestion === null
        ? html`<span class="skel" aria-hidden="true"></span>`
        : suggestion.more
          ? html`<span part="more">Type to narrow the list</span>`
          : nothing;

    return html`${field}${list}${note}`;
  }

  private numeric(draft: {family: 'numeric'; gte: number | null; lte: number | null}) {
    const column = this.resolvedStore?.get('meta')?.declaredScalars.find((c) => c.name === this.column);
    const date = column?.arrowType === 'timestamp_us';
    const toValue = (raw: string): number | null => {
      if (raw === '') return null;
      const n = date ? Date.parse(raw) * 1000 : Number(raw);
      return Number.isFinite(n) ? n : null;
    };
    const fromValue = (v: number | null): string => (v === null ? '' : date ? new Date(v / 1000).toISOString().slice(0, 10) : String(v));
    const bound = (which: 'gte' | 'lte') =>
      html`<input id=${which === 'gte' ? 'ctl' : nothing} part="entry" class="grow num" type=${date ? 'date' : 'number'}
        .value=${fromValue(draft[which])} aria-label=${`${this.column} ${which === 'gte' ? 'from' : 'to'}`}
        @change=${(e: Event) => this.change({...draft, [which]: toValue((e.target as HTMLInputElement).value)} as ColumnDraft, true)} />`;
    return html`<div class="ctl-row">${bound('gte')}<span class="to">to</span>${bound('lte')}</div>`;
  }
}

attachContextRoot();
defineOnce('tessera-filter', TesseraFilter);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-filter': TesseraFilter;
  }
}
