import {css, html, nothing} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {
  composeFilters,
  isPopulated,
  type CategoryValue,
  type ColumnDraft,
  type FilterOperandSet,
  type Refusal,
  type TextMode
} from '@tesseradb/client';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-filter column="…">` — one operand, rendered by its type from `meta` (design §5.3
 * tier 2), as the boards draw it: a text column is a search field with an *all words / phrase*
 * toggle; a category is a search field over the enumeration with the top few values as
 * checkboxes and *Show N more…* — never a scrolling list of 171; a number or a date is two
 * inputs with *to* between them; a keyword or string is a field with its operator.
 *
 * **A typed value is submitted, never validated against the enumeration.** A category's value
 * list is what `/v1/categories` was willing to list, and a key it did not list may still be one
 * this principal can filter by; an unresolvable one is an empty answer by contract (contracts
 * §3.2), indistinguishable from a value that does not exist. So the search field submits whatever
 * is typed on Enter, and the control never says "no such value". A refused enumeration renders as
 * a refusal beside the field, not as an absent control.
 *
 * The draft is local to the element while a user is typing; the store's draft re-seeds it only
 * when it changes under the element (a *clear all*). Typing is debounced; a tick, a mode or a
 * date lands at once. Emits `tessera-filterchange` with the composed expression.
 */

/** How long a typed control must be quiet before its change is sent. */
const TYPING_DEBOUNCE_MS = 350;
/** How many of a category's values show as checkboxes before *Show N more…*. */
const TOP_VALUES = 4;

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
  @property({attribute: false}) accessor values: CategoryValue[] | null = null;
  @property({attribute: false}) accessor valuesRefusal: Refusal | null = null;

  @state() accessor draft: ColumnDraft | null = null;
  @state() accessor search = '';
  @state() accessor expanded = false;
  private sent: ColumnDraft | null = null;
  private typing: ReturnType<typeof setTimeout> | null = null;

  private get resolvedOperand(): FilterOperandSet | null {
    if (this.operand) return this.operand;
    return this.resolvedStore?.get('meta')?.filterOperands.find((o) => o.column === this.column) ?? null;
  }

  private get resolvedValues(): {values: CategoryValue[] | null; refusal: Refusal | null} {
    if (this.values || this.valuesRefusal) return {values: this.values, refusal: this.valuesRefusal};
    const f = this.resolvedStore?.get('filters');
    return {values: f?.values[this.column] ?? null, refusal: f?.valueErrors[this.column] ?? null};
  }

  protected override onStoreChange(): void {
    // Re-seed from the store only when its draft moved under this control — a clear-all, or the
    // first meta — never while the user's own edit is the one in flight.
    const stored = this.resolvedStore?.get('filters').draft[this.column] ?? null;
    if (stored && stored !== this.sent && JSON.stringify(stored) !== JSON.stringify(this.draft)) {
      this.draft = structuredClone(stored);
      this.sent = stored;
    }
    if (this.resolvedOperand?.family === 'category' && !this.resolvedValues.values && !this.resolvedValues.refusal) {
      void this.resolvedStore?.loadFilterValues(this.column);
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

  private category(draft: {family: 'category'; keys: string[]}) {
    const {values, refusal} = this.resolvedValues;
    const chosen = new Set(draft.keys);
    const submit = () => {
      const key = this.search.trim();
      if (!key || chosen.has(key)) return;
      this.search = '';
      this.change({...draft, keys: [...draft.keys, key]}, true);
    };
    // The search field over the enumeration: whatever is typed narrows the list and, on Enter,
    // is submitted as a key. Never validated, never "no such value".
    const field = html`<div class="input">${icon('search', 14)}<input id="ctl" part="entry" type="search" autocomplete="off"
        aria-label=${`${this.column} value`} .value=${this.search}
        @input=${(e: Event) => (this.search = (e.target as HTMLInputElement).value)}
        @keydown=${(e: KeyboardEvent) => {
          if (e.key === 'Enter') submit();
        }} /></div>`;
    const listed = values ?? [];
    const typed = draft.keys.filter((k) => !listed.some((v) => v.key === k)).map((k) => ({code: -1, key: k, title: null}));
    const needle = this.search.trim().toLowerCase();
    const matches = (v: {key: string; title: string | null}) => needle === '' || v.key.toLowerCase().includes(needle) || (v.title ?? '').toLowerCase().includes(needle);
    // Chosen keys first, whether listed or typed, then the rest; the top few unless expanded.
    const ordered = [...typed, ...listed.filter((v) => chosen.has(v.key)), ...listed.filter((v) => !chosen.has(v.key))].filter(matches);
    const visible = this.expanded || needle !== '' ? ordered : ordered.slice(0, Math.max(TOP_VALUES, draft.keys.length));
    const hidden = ordered.length - visible.length;
    const ticks = html`<div part="values">
      ${repeat(
        visible,
        (v) => v.key,
        (v) => html`<label part="tick" class="check"
            ><input type="checkbox" .checked=${chosen.has(v.key)} value=${v.key}
              @change=${(e: Event) => {
                const on = (e.target as HTMLInputElement).checked;
                this.change({...draft, keys: on ? [...draft.keys, v.key] : draft.keys.filter((k) => k !== v.key)}, true);
              }} />
            <span class="t"
              >${v.title && v.title !== v.key
                ? html`${v.title}<span class="k">${v.key}</span>`
                : v.key}</span
            ></label
          >`
      )}
      ${hidden > 0
        ? html`<button part="more" type="button" @click=${() => (this.expanded = true)}>Show ${hidden.toLocaleString('en-GB')} more…</button>`
        : this.expanded && ordered.length > TOP_VALUES && needle === ''
          ? html`<button part="more" type="button" @click=${() => (this.expanded = false)}>Show fewer</button>`
          : nothing}
    </div>`;
    const note = refusal
      ? html`<span part="refusal" class="xs">${refusal.code}: values not listable</span>`
      : values === null
        ? html`<span class="skel" aria-hidden="true"></span>`
        : nothing;
    return html`${field}${ordered.length > 0 ? ticks : nothing}${note}`;
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
