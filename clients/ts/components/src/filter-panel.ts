import {css, html, nothing} from 'lit';
import {repeat} from 'lit/directives/repeat.js';
import {activeCount, emptyDraft, isPopulated, type ColumnDraft} from '@tesseradb/client';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';
import './filter.js';

/**
 * `<tessera-filter-panel>` — every operand `meta` offers, as `<tessera-filter>`s, with the
 * applied filters as chips with `×` and *Clear all* at the top (design §5.3 tier 2, the boards'
 * FILTERS panel). Which controls exist is the server's answer: a column absent from
 * `meta.filterOperands` is absent here, and a schema that adds one gains its control on the next
 * meta.
 *
 * Keyed rendering, so a store tick never rebuilds a control under the user's cursor.
 */
export class TesseraFilterPanel extends TesseraElement {
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
        margin-bottom: 12px;
      }
      [part='clear'] {
        text-transform: none;
        letter-spacing: 0;
        color: var(--tessera-accent);
        font-weight: 500;
        font-size: 12px;
      }
    `
  ];

  private chipText(column: string, draft: ColumnDraft): string {
    switch (draft.family) {
      case 'text':
        return `${column}: ${draft.mode === 'phrase' ? '“' + draft.query + '”' : draft.query}`;
      case 'string':
      case 'keyword':
        return `${column} ${draft.op} ${draft.needle}`;
      case 'category':
        return `${column}: ${draft.keys.join(', ')}`;
      case 'numeric': {
        const meta = this.resolvedStore?.get('meta');
        const date = meta?.declaredScalars.find((c) => c.name === column)?.arrowType === 'timestamp_us';
        const f = (v: number | null) => (v === null ? '…' : date ? new Date(v / 1000).toISOString().slice(0, 10) : String(v));
        return `${column}: ${f(draft.gte)} – ${f(draft.lte)}`;
      }
    }
  }

  private clear(column: string | null): void {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    if (!s || !meta) return;
    const empty = emptyDraft(meta.filterOperands);
    const next = column === null ? empty : {...s.get('filters').draft, [column]: empty[column]!};
    s.setFilters(next);
    emit(this, 'tessera-filterchange', {column, expr: null});
  }

  override render() {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel"><h2 part="title">Filters</h2>${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const operands = meta.filterOperands;
    if (operands.length === 0) return html`<div class="panel"><h2 part="title">Filters</h2><span part="state" data-state="empty">Nothing filterable</span></div>`;
    const draft = s.get('filters').draft;
    const active = activeCount(draft);
    const chips = Object.entries(draft).filter(([, d]) => isPopulated(d));
    return html`<div class="panel"><h2 part="title">Filters${active > 0 ? html`<button part="clear" type="button" @click=${() => this.clear(null)}>Clear all</button>` : nothing}</h2>
      <span part="state" data-state="shown"></span>
      ${chips.length > 0
        ? html`<div part="chips">${chips.map(([c, d]) => html`<span part="chip" class="chip">${this.chipText(c, d)}<button type="button" aria-label=${`Remove ${c} filter`} @click=${() => this.clear(c)}>${icon('close', 12)}</button></span>`)}</div>`
        : nothing}
      ${repeat(
        operands,
        (o) => o.column,
        (o) => html`<tessera-filter column=${o.column} .store=${s}></tessera-filter>`
      )}
    </div>`;
  }
}

attachContextRoot();
defineOnce('tessera-filter-panel', TesseraFilterPanel);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-filter-panel': TesseraFilterPanel;
  }
}
