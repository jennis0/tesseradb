import {css, html, nothing} from 'lit';
import {repeat} from 'lit/directives/repeat.js';
import {activeCount, emptyDraft, isPopulated, memberKey, withVerb, withoutMember, type ClauseVerb, type ColumnDraft, type MemberClause} from '@tesseradb/client';
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
 * **Every chip carries its verb** (`highlight-and-hierarchy.md` §5.2): the word says which of the
 * request's two expressions the clause joins — *filter*, and the map narrows to the matches;
 * *highlight*, and the map stays with the matches lit and the rest dulled — and clicking it moves
 * the clause without the predicate being re-entered. A `member_of` clause (an artifact named from
 * the card or the hierarchy panel) is a chip here too, and the same word moves it.
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

  /** The verb toggle a chip carries, and the word on it. */
  private verbToggle(verb: ClauseVerb, label: string, move: () => void) {
    const other: ClauseVerb = verb === 'filter' ? 'highlight' : 'filter';
    return html`<button
      class="verb"
      part="verb"
      type="button"
      data-verb=${verb}
      aria-label=${`${label} — ${other} instead`}
      title=${verb === 'filter' ? 'Filtering: the map narrows to the matches. Highlight instead' : 'Highlighting: the map stays and the matches are lit. Filter instead'}
      @click=${move}
    >
      ${icon(verb === 'filter' ? 'filter' : 'highlight', 11)}${verb}
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

  /** What a `member_of` chip says: the artifact's name where the map served it, else its layer. */
  private memberText(clause: MemberClause): string {
    // The clause's own label first: an artifact of a filter layer is never in the served set, so
    // that is the only place a name for it can come from (§5.4).
    const served = this.resolvedStore?.get('artifacts').served.find((a) => a.tesseraId === clause.artifact && a.layer === clause.layer);
    const name = clause.label ?? served?.content[0] ?? served?.key ?? clause.layer;
    return clause.outside ? `outside ${name}` : name;
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

  override render() {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel"><h2 part="title">Filters</h2>${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const operands = meta.filterOperands;
    if (operands.length === 0) return html`<div class="panel"><h2 part="title">Filters</h2><span part="state" data-state="empty">Nothing filterable</span></div>`;
    const {draft, members} = s.get('filters');
    const active = activeCount(draft);
    const chips = Object.entries(draft).filter(([, d]) => isPopulated(d));
    return html`<div class="panel"><h2 part="title">Filters${active > 0 || members.length > 0 ? html`<button part="clear" type="button" @click=${() => this.clear(null)}>Clear all</button>` : nothing}</h2>
      <span part="state" data-state="shown"></span>
      ${chips.length > 0 || members.length > 0
        ? html`<div part="chips">
            ${chips.map(
              ([c, d]) => html`<span part="chip" class="chip" data-verb=${d.verb} data-column=${c}
                >${this.verbToggle(d.verb, c, () => this.moveColumn(c, d.verb === 'filter' ? 'highlight' : 'filter'))}${this.chipText(c, d)}<button
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
                >${this.verbToggle(m.verb, this.memberText(m), () => this.moveMember(m, m.verb === 'filter' ? 'highlight' : 'filter'))}${this.memberText(m)}<button
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
