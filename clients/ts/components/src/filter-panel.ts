import {css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {activeCount, emptyDraft, isPopulated, withMember, withVerb, withoutClause, withoutMember, type ClauseVerb, type ColumnDraft, type MemberClause} from '@tesseradb/client';
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
 * every count to the matches, and a highlight keeps the map and lights the matches. A column or an
 * artifact can hold a clause in each, and then has two chips. A highlight chip says "highlight"; a
 * filter chip carries no mark. The controls edit the filter clauses; a highlight shows only as its
 * chip. A `member_of` clause (an artifact chosen on the artifact card or in the hierarchy) is a
 * chip too.
 *
 * Each chip's toggle moves its clause to the other position. Where that position already holds a
 * clause on the same column, the two merge as `withVerb` says; on the same artifact, the moved
 * clause replaces the one there. Removing a chip empties its clause and leaves the other position's
 * alone. Clear all empties every control in both positions and drops every `member_of` clause. `chips-only` leaves the controls out, and renders nothing while no
 * clause is applied.
 *
 * @summary Every filter control, with the applied clauses as chips.
 * @tagname tessera-filter-panel
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A
 *   column's chip moved between filter and highlight (with `verb`, its new position), a column's
 *   chip was removed (with `verb`, the position it was in, and `expr` null), or Clear all was
 *   pressed (with `column` and `expr` null). Moving or removing
 *   a `member_of` chip fires nothing. Each inner control fires its own as well.
 * @csspart title - The heading, holding Clear all at its right.
 * @csspart clear - The Clear all button, shown while any clause is applied.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - The words "View refused", with `data-code`, in the refused state.
 * @csspart chips - The applied clauses.
 * @csspart chip - One applied clause, with `data-verb` (`filter` or `highlight`) and `data-column`
 *   or `data-artifact`.
 * @csspart verb - The button that moves a clause to the other position: the word "highlight" on a
 *   highlight chip, and on a filter chip the highlight icon, shown on hover or focus.
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
      .chip .verb[data-verb='filter'] {
        margin: 0;
        padding: 0 2px;
        background: none;
        opacity: 0;
      }
      .chip:hover .verb[data-verb='filter'],
      .chip:focus-within .verb[data-verb='filter'] {
        opacity: 1;
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

  /**
   * The verb toggle a chip carries. A highlight chip is marked with the word; a filter chip, the
   * usual kind, carries no mark, and its toggle shows on hover or focus as the highlight icon.
   */
  private verbToggle(verb: ClauseVerb, label: string, move: () => void) {
    const other: ClauseVerb = verb === 'filter' ? 'highlight' : 'filter';
    return html`<button
      class="verb"
      part="verb"
      type="button"
      data-verb=${verb}
      aria-label=${`${label}; ${other} instead`}
      title=${verb === 'filter' ? 'Highlight instead' : 'Filter instead'}
      @click=${move}
    >
      ${verb === 'filter' ? icon('highlight', 12) : html`${icon('highlight', 11)}highlight`}
    </button>`;
  }

  private moveColumn(column: string, to: ClauseVerb): void {
    const s = this.resolvedStore;
    if (!s) return;
    s.setFilters(withVerb(s.get('filters').draft, column, to));
    emit(this, 'tessera-filterchange', {column, verb: to});
  }

  private moveMember(clause: MemberClause, to: ClauseVerb): void {
    const s = this.resolvedStore;
    if (!s) return;
    s.setMembers(withMember(withoutMember(s.get('filters').members, clause.layer, clause.artifact, clause.verb), {...clause, verb: to}));
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
    const served = this.resolvedStore?.get('artifacts').served.find((a) => a.tesseraId === clause.artifact && a.layer === clause.layer);
    const name = clause.label ?? served?.content[0] ?? UNNAMED;
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
                >${verb === 'highlight' ? this.verbToggle(verb, c, () => this.moveColumn(c, 'filter')) : nothing}${this.chipText(c, d)}${verb === 'filter' ? this.verbToggle(verb, c, () => this.moveColumn(c, 'highlight')) : nothing}<button
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
                >${m.verb === 'highlight' ? this.verbToggle(m.verb, this.memberText(m), () => this.moveMember(m, 'filter')) : nothing}${this.memberText(m)}${m.verb === 'filter' ? this.verbToggle(m.verb, this.memberText(m), () => this.moveMember(m, 'highlight')) : nothing}<button
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
