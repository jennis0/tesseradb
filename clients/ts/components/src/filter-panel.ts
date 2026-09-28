import {css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {activeCount, emptyDraft, isPopulated, memberKey, withVerb, withoutMember, type ClauseVerb, type ColumnDraft, type MemberClause} from '@tesseradb/client';
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
 * every count to the matches, and a highlight keeps the map and lights the matches. A highlight
 * chip says "highlight"; a filter chip carries no mark. Each chip's toggle moves the clause to the
 * other position. A `member_of` clause (an artifact chosen on the artifact card or in the
 * hierarchy) is a chip too.
 *
 * Removing a chip empties its control and keeps its position. Clear all empties every control and
 * drops every `member_of` clause. `chips-only` leaves the controls out, and renders nothing while no
 * clause is applied.
 *
 * @summary Every filter control, with the applied clauses as chips.
 * @tagname tessera-filter-panel
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A
 *   column's chip moved between filter and highlight (with `verb`), a column's chip was removed
 *   (with `expr` null), or Clear all was pressed (with `column` and `expr` null). Moving or removing
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
    const key = memberKey(clause.layer, clause.artifact);
    s.setMembers(s.get('filters').members.map((c) => (memberKey(c.layer, c.artifact) === key ? {...c, verb: to} : c)));
  }

  private clearMember(clause: MemberClause): void {
    const s = this.resolvedStore;
    if (!s) return;
    s.setMembers(withoutMember(s.get('filters').members, clause.layer, clause.artifact));
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

  private clear(column: string | null): void {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    if (!s || !meta) return;
    const empty = emptyDraft(meta.filterOperands);
    const held = s.get('filters').draft;
    // Clearing a control empties its predicate and keeps its position: a viewer who moved a clause
    // to the highlight and then retyped it means the highlight.
    const keep = (c: string) => ({...empty[c]!, verb: held[c]?.verb ?? 'filter'}) as ColumnDraft;
    const next = column === null ? empty : {...held, [column]: keep(column)};
    s.setFilters(next);
    if (column === null) s.setMembers([]);
    emit(this, 'tessera-filterchange', {column, expr: null});
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel"><h2 part="title">Filters</h2>${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const operands = meta.filterOperands;
    if (operands.length === 0) return html`<div class="panel"><h2 part="title">Filters</h2><span part="state" data-state="empty">Nothing filterable</span></div>`;
    const {draft, members} = s.get('filters');
    const active = activeCount(draft);
    const chips = Object.entries(draft).filter(([, d]) => isPopulated(d));
    if (this.chipsOnly && chips.length === 0 && members.length === 0) return nothing;
    return html`<div class="panel"><h2 part="title">Filters${active > 0 || members.length > 0 ? html`<button part="clear" class="quiet" type="button" @click=${() => this.clear(null)}>Clear all</button>` : nothing}</h2>
      <span part="state" data-state="shown"></span>
      ${chips.length > 0 || members.length > 0
        ? html`<div part="chips">
            ${chips.map(
              ([c, d]) => html`<span part="chip" class="chip" data-verb=${d.verb} data-column=${c}
                >${d.verb === 'highlight' ? this.verbToggle(d.verb, c, () => this.moveColumn(c, 'filter')) : nothing}${this.chipText(c, d)}${d.verb === 'filter' ? this.verbToggle(d.verb, c, () => this.moveColumn(c, 'highlight')) : nothing}<button
                  type="button"
                  aria-label=${`Remove ${c} filter`}
                  @click=${() => this.clear(c)}
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
