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
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-filter column="…">` — one operand, rendered by its type from `meta` (design §5.3
 * tier 2): a value picker for a category, a range for a number or a datetime, words-or-phrase
 * for text, a value for a keyword or string. Calls `setFilters` with the recomposed expression.
 *
 * **A typed value is submitted, never validated against the enumeration.** A category's value
 * list is what `/v1/categories` was willing to list, and a key it did not list may still be one
 * this principal can filter by; an unresolvable one is an empty answer by contract (contracts
 * §3.2), indistinguishable from a value that does not exist. So the control offers a free entry
 * beside the ticks, submits whatever is typed, and never says "no such value". A refused
 * enumeration renders as a refusal beside the entry, not as an absent control — not listable is
 * not the same as not filterable.
 *
 * The draft is local to the element while a user is typing; the store's draft re-seeds it only
 * when it changes under the element (a *clear all*). Typing is debounced; a tick, a mode or a
 * date lands at once, having no intermediate states. Emits `tessera-filterchange` with the
 * composed expression.
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
        margin-bottom: calc(var(--tessera-space) * 1.6);
        padding-left: calc(var(--tessera-space) * 1.3);
        border-left: 2px solid var(--tessera-border);
      }
      :host([data-on]) {
        border-left-color: var(--tessera-accent);
      }
      [part='label'] {
        display: block;
        margin-bottom: 2px;
      }
      :host([data-on]) [part='label'] {
        color: var(--tessera-accent);
      }
      .ctl-row {
        display: flex;
        align-items: center;
        gap: var(--tessera-space);
        margin-bottom: 4px;
      }
      .ctl-row .grow {
        flex: 1 1 0;
        min-width: 0;
      }
      [part='values'] {
        max-height: 96px;
        overflow-y: auto;
        border: 1px solid var(--tessera-border);
        border-radius: var(--tessera-radius);
        padding: 4px 6px;
        margin-bottom: 4px;
      }
      [part='tick'] {
        display: flex;
        align-items: baseline;
        gap: var(--tessera-space);
        cursor: pointer;
        white-space: nowrap;
      }
      [part='tick'] input {
        margin: 0;
      }
    `
  ];

  @property() accessor column = '';
  /** The operand set by property, for a host with no store on the page. */
  @property({attribute: false}) accessor operand: FilterOperandSet | null = null;
  @property({attribute: false}) accessor values: CategoryValue[] | null = null;
  @property({attribute: false}) accessor valuesRefusal: Refusal | null = null;

  @state() accessor draft: ColumnDraft | null = null;
  private sent: ColumnDraft | null = null;
  private typing: ReturnType<typeof setTimeout> | null = null;
  private entry = '';

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

  override render() {
    const o = this.resolvedOperand;
    if (!o) return nothing;
    const draft = this.draft ?? this.emptyDraft(o);
    return html`<label part="label" for="ctl">${o.column}</label>${this.body(o, draft)}`;
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
    const modes: [TextMode, string][] = [
      ['all', 'all words'],
      ['any', 'any word']
    ];
    if (o.operands.includes('phrase')) modes.push(['phrase', 'exact phrase']);
    return html`<div class="ctl-row">
      <input id="ctl" part="entry" class="grow" type="search" .value=${draft.query} placeholder="words to match" autocomplete="off"
        aria-label=${`${o.column} words`}
        @input=${(e: Event) => this.change({...draft, query: (e.target as HTMLInputElement).value}, false)} />
      <select part="mode" aria-label=${`${o.column} mode`} .value=${draft.mode}
        @change=${(e: Event) => this.change({...draft, mode: (e.target as HTMLSelectElement).value as TextMode}, true)}>
        ${modes.map(([v, t]) => html`<option value=${v} ?selected=${draft.mode === v}>${t}</option>`)}
      </select>
    </div>`;
  }

  private string(draft: {family: 'string' | 'keyword'; needle: string; op: 'eq' | 'prefix' | 'contains'}) {
    return html`<div class="ctl-row">
      <input id="ctl" part="entry" class="grow" type="search" .value=${draft.needle} placeholder="value" autocomplete="off"
        aria-label=${`${this.column} value`}
        @input=${(e: Event) => this.change({...draft, needle: (e.target as HTMLInputElement).value}, false)} />
      <select part="mode" aria-label=${`${this.column} operator`} .value=${draft.op}
        @change=${(e: Event) => this.change({...draft, op: (e.target as HTMLSelectElement).value as 'eq' | 'prefix' | 'contains'}, true)}>
        ${(['contains', 'prefix', 'eq'] as const).map((op) => html`<option value=${op} ?selected=${draft.op === op}>${op}</option>`)}
      </select>
    </div>`;
  }

  private category(draft: {family: 'category'; keys: string[]}) {
    const {values, refusal} = this.resolvedValues;
    const chosen = new Set(draft.keys);
    const submit = () => {
      const key = this.entry.trim();
      if (!key || chosen.has(key)) return;
      this.entry = '';
      this.change({...draft, keys: [...draft.keys, key]}, true);
    };
    // The free entry: whatever is typed is submitted as a key. Never validated, never "no such value".
    const entry = html`<div class="ctl-row">
      <input id="ctl" part="entry" class="grow" type="search" placeholder="a key to filter by" autocomplete="off"
        aria-label=${`${this.column} key`} .value=${this.entry}
        @input=${(e: Event) => (this.entry = (e.target as HTMLInputElement).value)}
        @keydown=${(e: KeyboardEvent) => {
          if (e.key === 'Enter') submit();
        }} />
      <button part="submit" type="button" @click=${submit}>add</button>
    </div>`;
    // Selected keys first, whether listed or typed, so a long list stays legible once chosen.
    const listed = values ?? [];
    const typed = draft.keys.filter((k) => !listed.some((v) => v.key === k)).map((k) => ({code: -1, key: k, title: null}));
    const ordered = [...typed, ...listed.filter((v) => chosen.has(v.key)), ...listed.filter((v) => !chosen.has(v.key))];
    const ticks = html`<div part="values">
      ${repeat(
        ordered,
        (v) => v.key,
        (v) => html`<label part="tick"
            ><input type="checkbox" .checked=${chosen.has(v.key)} value=${v.key}
              @change=${(e: Event) => {
                const on = (e.target as HTMLInputElement).checked;
                this.change({...draft, keys: on ? [...draft.keys, v.key] : draft.keys.filter((k) => k !== v.key)}, true);
              }} />
            ${v.title && v.title !== v.key ? `${v.key} — ${v.title}` : v.key}</label
          >`
      )}
    </div>`;
    const note = refusal
      ? html`<span part="refusal">${refusal.code}: values not listable — a key typed above is still filtered by</span>`
      : values === null
        ? html`<span class="muted">listing values…</span>`
        : html`<span class="muted">${draft.keys.length > 0 ? `${draft.keys.length} chosen — any of` : `${values.length} listable`}</span>`;
    return html`${entry}${ordered.length > 0 ? ticks : nothing}${note}`;
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
    const bound = (which: 'gte' | 'lte', placeholder: string) =>
      html`<input id=${which === 'gte' ? 'ctl' : nothing} part="entry" class="grow" type=${date ? 'date' : 'number'}
        .value=${fromValue(draft[which])} placeholder=${placeholder} aria-label=${`${this.column} ${placeholder}`}
        @change=${(e: Event) => this.change({...draft, [which]: toValue((e.target as HTMLInputElement).value)}, true)} />`;
    return html`<div class="ctl-row">${bound('gte', 'min')}<span class="muted">to</span>${bound('lte', 'max')}</div>`;
  }
}

attachContextRoot();
defineOnce('tessera-filter', TesseraFilter);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-filter': TesseraFilter;
  }
}
