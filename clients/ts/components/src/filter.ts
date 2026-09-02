import {css, html, nothing, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {
  composeFilters,
  isPopulated,
  type ColumnDraft,
  type FilterOperandSet,
  type MatchSpan,
  type Refusal,
  type SuggestValue,
  type TextMode
} from '@tesseradb/client';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-filter column="…">` — one operand, rendered by its type from `meta` (design §5.3
 * tier 2), as the boards draw it: a text column is a search field with an *all words / phrase*
 * toggle; a category is a typeahead over `/v1/categories/{column}/suggest`
 * (`value-suggestion.md`) — a search field, the matched values with the matched span marked, the
 * chosen ones as chips above it; a number or a date is two inputs with *to* between them; a
 * keyword or string is a field with its operator.
 *
 * **A typed value is submitted, never validated against the suggestion page.** A category's
 * suggestions are what `/v1/categories/{column}/suggest` was willing to offer, and a key it did
 * not offer may still be one this principal can filter by; an unresolvable one is an empty answer
 * by contract (contracts §3.2), indistinguishable from a value that does not exist. So the search
 * field submits whatever is typed on Enter, and the control never says "no such value". A refused
 * suggestion renders as a refusal beside the field, not as an absent control.
 *
 * **The suggestion page is rendered only while it answers the box in front of it.** The store
 * echoes `q` back on the projection precisely so a stale page — a slower response to an earlier
 * keystroke, landing after a faster response to a later one — is never mistaken for an answer to
 * what is now typed; this element applies the same `q` check the store already used to decide
 * whether to keep the page at all, because a page can go stale here too, between the store's tick
 * and this element's next render, in the case fewest visits: a very fast keystroke arriving inside
 * one microtask queue flush.
 *
 * The draft is local to the element while a user is typing; the store's draft re-seeds it only
 * when it changes under the element (a *clear all*). Typing asks the store's typeahead action on
 * every keystroke, which debounces and single-flights it; a tick, a mode or a date lands at once.
 * Emits `tessera-filterchange` with the composed expression.
 */

/** How long a typed control must be quiet before its change is sent. */
const TYPING_DEBOUNCE_MS = 350;

export class TesseraFilter extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
        margin-top: 12px;
      }
      :host(:first-of-type) {
        margin-top: 0;
      }
      [part='label'] {
        display: block;
        margin-bottom: 6px;
        font-size: 11px;
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
      [part='value-chips'] {
        display: flex;
        flex-wrap: wrap;
        gap: 4px;
        margin-top: 6px;
      }
      [part='values'] {
        margin-top: 4px;
        display: flex;
        flex-direction: column;
        gap: 2px;
      }
      [part='tick'] .t {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      /** The matched span, in the served string, exactly where the server said it sits. */
      [part='tick'] mark {
        background: var(--tessera-accent-soft);
        color: var(--tessera-accent);
        border-radius: 2px;
      }
      /**
       * **The title leads and the key follows it, muted**, where the two differ. A value's key is
       * what the filter is written in and it is not always what the value is called: a boundary
       * set keys its divisions by uuid, so a key-then-title line put 36 characters of
       * hexadecimal in front of every name and a column of them read as a column of nothing. Where a key is the name
       * — an arXiv category, a country code — the two are one string and only it is drawn.
       */
      [part='tick'] .k {
        margin-left: 0.45em;
        opacity: 0.55;
        font-size: 0.85em;
        font-variant-numeric: tabular-nums;
      }
      [part='more'] {
        text-align: left;
        height: 24px;
        color: var(--tessera-ink-3);
        font-size: 12px;
      }
      .to {
        color: var(--tessera-ink-3);
      }
      input.mono {
        font-family: var(--tessera-font-mono);
      }
    `
  ];

  @property() accessor column = '';
  /** The operand set by property, for a host with no store on the page. */
  @property({attribute: false}) accessor operand: FilterOperandSet | null = null;

  @state() accessor draft: ColumnDraft | null = null;
  @state() accessor search = '';
  /**
   * The title last seen for a chosen key, so a chip shows a name rather than a bare key once its
   * value has scrolled out of the current suggestion page. Filled in the moment a value is picked
   * from a page that carried one; never fetched for its own sake — a chosen key with no title on
   * record renders as its key, exactly as an unresolved one would.
   */
  @state() accessor labels: Record<string, string> = {};
  private sent: ColumnDraft | null = null;
  private typing: ReturnType<typeof setTimeout> | null = null;

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

  protected override onStoreChange(): void {
    // Re-seed from the store only when its draft moved under this control — a clear-all, or the
    // first meta — never while the user's own edit is the one in flight.
    const stored = this.resolvedStore?.get('filters').draft[this.column] ?? null;
    if (stored && stored !== this.sent && JSON.stringify(stored) !== JSON.stringify(this.draft)) {
      this.draft = structuredClone(stored);
      this.sent = stored;
    }
    // The picker's list before anything is typed (`value-suggestion.md` §4): an empty `q` matches
    // every value, so the first ask is for `this.search` as it stands — `''` on mount — rather
    // than a separate enumeration call. Asked once per box's worth of unanswered state, the same
    // guard `loadFilterValues` used for the enumeration: nothing landed yet, and nothing refused.
    const filters = this.resolvedStore?.get('filters');
    if (this.resolvedOperand?.family === 'category' && filters && !filters.suggestions[this.column] && !filters.suggestErrors[this.column]) {
      this.resolvedStore?.suggest(this.column, this.search);
    }
    super.onStoreChange();
  }

  protected override updated(): void {
    if (this.draft && isPopulated(this.draft)) this.setAttribute('data-on', '');
    else this.removeAttribute('data-on');
  }

  private emptyDraft(o: FilterOperandSet): ColumnDraft {
    switch (o.family) {
      case 'text':
        return {family: 'text', query: '', mode: 'all'};
      case 'string':
      case 'keyword':
        return {family: o.family, needle: '', op: 'contains'};
      case 'category':
        return {family: 'category', keys: []};
      case 'numeric':
        return {family: 'numeric', gte: null, lte: null};
    }
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

  /** The column's name for a label: `submitted_at` reads as *Submitted at*. */
  private heading(): string {
    const name = this.column.replace(/_/g, ' ');
    return name.charAt(0).toUpperCase() + name.slice(1);
  }

  override render() {
    const o = this.resolvedOperand;
    if (!o) return nothing;
    const draft = this.draft ?? this.emptyDraft(o);
    return html`<label part="label" class="muted" for="ctl">${this.heading()}</label>${this.body(o, draft)}`;
  }

  private body(o: FilterOperandSet, draft: ColumnDraft) {
    switch (draft.family) {
      case 'text':
        return this.text(o, draft);
      case 'string':
      case 'keyword':
        return this.string(draft);
      case 'category':
        return this.category(draft);
      case 'numeric':
        return this.numeric(draft);
    }
  }

  private text(o: FilterOperandSet, draft: {family: 'text'; query: string; mode: TextMode}) {
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

  private string(draft: {family: 'string' | 'keyword'; needle: string; op: 'eq' | 'prefix' | 'contains'}) {
    return html`<div class="ctl-row">
      <div class="input grow">${icon('search', 14)}<input id="ctl" part="entry" type="search" .value=${draft.needle} autocomplete="off"
        aria-label=${`${this.column} value`}
        @input=${(e: Event) => this.change({...draft, needle: (e.target as HTMLInputElement).value}, false)} /></div>
      <select part="mode" style="width:auto" aria-label=${`${this.column} operator`} .value=${draft.op}
        @change=${(e: Event) => this.change({...draft, op: (e.target as HTMLSelectElement).value as 'eq' | 'prefix' | 'contains'}, true)}>
        ${(['contains', 'prefix', 'eq'] as const).map((op) => html`<option value=${op} ?selected=${draft.op === op}>${op}</option>`)}
      </select>
    </div>`;
  }

  /**
   * `field`'s text, the matched span marked where the server said it sits — in **characters of
   * the served string**, so this never re-runs the fold (`value-suggestion.md` §5.1). Split on
   * code points rather than UTF-16 units: the offsets are characters, and a naive `.slice` would
   * cut a surrogate pair in half on any served string outside the basic plane.
   */
  private markedField(v: SuggestValue, field: MatchSpan['field'], text: string): TemplateResult | string {
    if (v.match.field !== field) return text;
    const chars = [...text];
    const {start, len} = v.match;
    return html`${chars.slice(0, start).join('')}<mark>${chars.slice(start, start + len).join('')}</mark>${chars.slice(start + len).join('')}`;
  }

  /** One suggested value's text: title leading, key muted after it, as a chosen value's does. */
  private suggestionText(v: SuggestValue): TemplateResult {
    const title = v.title ?? v.key;
    return v.title && v.title !== v.key
      ? html`${this.markedField(v, 'title', title)}<span class="k">${this.markedField(v, 'key', v.key)}</span>`
      : html`${this.markedField(v, 'key', v.key)}`;
  }

  private category(draft: {family: 'category'; keys: string[]}) {
    const suggestion = this.resolvedSuggestion;
    const refusal = this.resolvedSuggestRefusal;
    const chosen = new Set(draft.keys);

    const pick = (v: SuggestValue) => {
      if (v.title) this.labels = {...this.labels, [v.key]: v.title};
      this.change(chosen.has(v.key) ? {...draft, keys: draft.keys.filter((k) => k !== v.key)} : {...draft, keys: [...draft.keys, v.key]}, true);
    };
    const remove = (key: string) => this.change({...draft, keys: draft.keys.filter((k) => k !== key)}, true);

    // What is typed is submitted on Enter whether or not it matched a suggestion (contracts
    // §3.2): a key the page never offered may still be one this principal can filter by, and an
    // unresolvable one is an empty answer, indistinguishable from one that does not exist — never
    // a rejected keystroke.
    const submit = () => {
      const key = this.search.trim();
      if (!key || chosen.has(key)) return;
      this.search = '';
      this.resolvedStore?.suggest(this.column, '');
      this.change({...draft, keys: [...draft.keys, key]}, true);
    };
    const field = html`<div class="input">${icon('search', 14)}<input id="ctl" part="entry" type="search" autocomplete="off"
        aria-label=${`${this.column} value`} .value=${this.search}
        @input=${(e: Event) => {
          this.search = (e.target as HTMLInputElement).value;
          this.resolvedStore?.suggest(this.column, this.search);
        }}
        @keydown=${(e: KeyboardEvent) => {
          if (e.key === 'Enter') submit();
        }} /></div>`;

    const chips =
      draft.keys.length > 0
        ? html`<div part="value-chips">
            ${repeat(
              draft.keys,
              (k) => k,
              (k) => html`<span part="value-chip" class="chip">${this.labels[k] ?? k}<button type="button" aria-label=${`Remove ${k}`} @click=${() => remove(k)}>${icon('close', 12)}</button></span>`
            )}
          </div>`
        : nothing;

    const rows = suggestion?.values ?? [];
    const list =
      rows.length > 0
        ? html`<div part="values" class="list">
            ${repeat(
              rows,
              (v) => v.code,
              (v) => html`<div part="tick" class="item" role="option" aria-selected=${chosen.has(v.key) ? 'true' : 'false'} @click=${() => pick(v)}>
                  <span class="t">${this.suggestionText(v)}</span>
                </div>`
            )}
          </div>`
        : nothing;

    const note = refusal
      ? html`<span part="refusal" class="xs">${refusal.code}: values not listable</span>`
      : suggestion === null
        ? html`<span class="skel" aria-hidden="true"></span>`
        : suggestion.more
          ? html`<span part="more" class="xs">type more to narrow</span>`
          : nothing;

    return html`${field}${chips}${list}${note}`;
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
      html`<input id=${which === 'gte' ? 'ctl' : nothing} part="entry" class="grow mono" type=${date ? 'date' : 'number'}
        .value=${fromValue(draft[which])} aria-label=${`${this.column} ${which === 'gte' ? 'from' : 'to'}`}
        @change=${(e: Event) => this.change({...draft, [which]: toValue((e.target as HTMLInputElement).value)}, true)} />`;
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
