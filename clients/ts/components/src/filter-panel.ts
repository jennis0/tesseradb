import {css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {activeCount, artifactName, emptyDraft, isPopulated, withoutClause, withoutMember, type ClauseVerb, type ColumnDraft, type MemberClause} from '@tesseradb/client';
import type {TesseraFilter} from './filter.js';
import {TesseraElement, UNNAMED, columnCaption, dateText, emit, keyTitle} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {exportparts} from './parts.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';
import './filter.js';

/** Each control's parts, forwarded as `filter-<part>` (`parts.ts`). */
const FILTER_PARTS = exportparts('filter');

/**
 * A `<tessera-filter>` for every column `meta.filterOperands` lists, under the clauses applied, as
 * chips, and a Clear all button. A clause is in one of two positions: a filter narrows the map and
 * every count to the matches, and a highlight lights the matches among what the filter keeps. A
 * column or an artifact can hold a clause in each. The two are independent: neither changes the
 * other, and each has its own chip. A highlight chip says "highlight"; a filter chip carries no
 * mark. A `member_of` clause (an artifact chosen on the artifact card or in the hierarchy) is a
 * chip too.
 *
 * Each control has a Filter / Highlight switch choosing which of its column's two clauses it edits.
 * Pressing a column's chip switches that column's control to the chip's position, scrolls it into
 * view and focuses it; under `chips-only`, where there are no controls, it fires
 * `tessera-chipopen` and does the same once the controls are shown. Removing a chip empties its
 * clause and leaves the other position's alone. Clear all empties every control in both positions
 * and drops every `member_of` clause. `chips-only` leaves the controls out, and renders nothing
 * while no clause is applied.
 *
 * @summary Every filter control, with the applied clauses as chips.
 * @tagname tessera-filter-panel
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A
 *   column's chip was removed (with `verb`, the position it was in, and `expr` null), or Clear all
 *   was pressed (with `column` and `expr` null). Removing a `member_of` chip fires nothing. Each
 *   inner control fires its own as well.
 * @fires {CustomEvent<TesseraEventDetails['tessera-chipopen']>} tessera-chipopen - A column's chip
 *   was pressed, naming the column and the position its control is to edit.
 * @csspart title - The heading, holding Clear all at its right.
 * @csspart clear - The Clear all button, shown while any clause is applied.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - The words "View refused", with `data-code`, in the refused state.
 * @csspart chips - The applied clauses.
 * @csspart chip - One applied clause, with `data-verb` (`filter` or `highlight`) and `data-column`
 *   or `data-artifact`.
 * @csspart edit - The button that is a column chip's text, which opens its control.
 * @csspart verb - The word "highlight" on a highlight chip.
 * @csspart filter-<part> - A part of an inner `<tessera-filter>`, forwarded under a `filter-`
 *   prefix: `filter-entry`, `filter-tick`, and so on.
 */
export class TesseraFilterPanel extends TesseraElement {
  /** Renders the heading and the chips without the controls, and nothing while no clause is applied. */
  @property({type: Boolean, attribute: 'chips-only'}) accessor chipsOnly = false;

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
      .chip .edit {
        display: inline-flex;
        flex: 0 1 auto;
        align-items: center;
        gap: 6px;
        color: inherit;
        font: inherit;
        text-align: left;
      }
      [part='chips']:last-child {
        margin-bottom: 0;
      }
      tessera-filter {
        padding: 12px 0 14px;
        border-top: 1px solid var(--_tessera-line-2);
      }
    `
  ];

  private chipText(column: string, draft: ColumnDraft): string {
    const caption = columnCaption(column);
    switch (draft.family) {
      case 'text':
        return `${caption}: ${draft.mode === 'phrase' ? '“' + draft.query + '”' : draft.query}`;
      case 'keyword':
        return `${caption} ${draft.op === 'eq' ? 'is' : draft.op === 'prefix' ? 'starts with' : 'contains'} ${draft.needle}`;
      case 'category':
        return `${caption}: ${draft.keys.map((k) => keyTitle(this.resolvedStore, column, k)).join(', ')}`;
      case 'numeric': {
        const meta = this.resolvedStore?.get('meta');
        const date = meta?.declaredScalars.find((c) => c.name === column)?.arrowType === 'timestamp_us';
        const f = (v: number) => (date ? dateText(v) : v.toLocaleString('en-GB'));
        if (draft.lte === null) return `${caption} ≥ ${f(draft.gte!)}`;
        if (draft.gte === null) return `${caption} ≤ ${f(draft.lte)}`;
        // A date range is words either side, so the dash takes spaces; a number range does not.
        return `${caption} ${f(draft.gte)}${date ? ' – ' : '–'}${f(draft.lte)}`;
      }
    }
  }

  /** The mark a highlight chip carries: the word, in the highlight colour. */
  private mark(verb: ClauseVerb) {
    return verb === 'highlight' ? html`<span class="verb" part="verb">${icon('highlight', 11)}highlight</span>` : nothing;
  }

  /** The chip last pressed, whose control opens once the controls are drawn. */
  private editing: {column: string; verb: ClauseVerb} | null = null;

  private edit(column: string, verb: ClauseVerb): void {
    this.editing = {column, verb};
    if (this.chipsOnly) emit(this, 'tessera-chipopen', {column, verb});
    else this.requestUpdate();
  }

  protected override updated(): void {
    if (!this.editing || this.chipsOnly) return;
    const {column, verb} = this.editing;
    const control = Array.from(this.renderRoot.querySelectorAll<TesseraFilter>('tessera-filter')).find((f) => f.column === column);
    if (!control) return;
    this.editing = null;
    control.verb = verb;
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

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel"><h2 part="title">Filters</h2>${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const operands = meta.filterOperands;
    if (operands.length === 0) return html`<div class="panel"><h2 part="title">Filters</h2><span part="state" data-state="empty">Nothing filterable</span></div>`;
    const {draft, members} = s.get('filters');
    const active = activeCount(draft);
    // A column's filter chip and then its highlight chip, the columns in the draft's order.
    const columns = [...new Set([...Object.keys(draft.filter), ...Object.keys(draft.highlight)])];
    const chips = columns.flatMap((c) =>
      (['filter', 'highlight'] as const).flatMap((verb) => {
        const d = draft[verb][c];
        return d && isPopulated(d) ? [{c, verb, d}] : [];
      })
    );
    if (this.chipsOnly && chips.length === 0 && members.length === 0) return nothing;
    return html`<div class="panel"><h2 part="title">Filters${active > 0 || members.length > 0 ? html`<button part="clear" class="quiet" type="button" @click=${() => this.clearAll()}>Clear all</button>` : nothing}</h2>
      <span part="state" data-state="shown"></span>
      ${chips.length > 0 || members.length > 0
        ? html`<div part="chips">
            ${chips.map(
              ({c, verb, d}) => html`<span part="chip" class="chip" data-verb=${verb} data-column=${c}
                ><button part="edit" class="edit" type="button" title=${`Edit the ${verb}`} @click=${() => this.edit(c, verb)}>${this.mark(verb)}${this.chipText(c, d)}</button><button
                  type="button"
                  aria-label=${`Remove ${c} ${verb}`}
                  @click=${() => this.clearColumn(c, verb)}
                >
                  ${icon('close', 12)}
                </button></span
              >`
            )}
            ${members.map(
              (m) => html`<span part="chip" class="chip" data-verb=${m.verb} data-artifact=${String(m.artifact)}
                >${this.mark(m.verb)}${this.memberText(m)}<button
                  type="button"
                  aria-label=${`Remove ${this.memberText(m)}`}
                  @click=${() => this.clearMember(m)}
                >
                  ${icon('close', 12)}
                </button></span
              >`
            )}
          </div>`
        : nothing}
      ${this.chipsOnly
        ? nothing
        : repeat(
            operands,
            (o) => o.column,
            (o) => html`<tessera-filter exportparts=${FILTER_PARTS} column=${o.column} .store=${s}></tessera-filter>`
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
