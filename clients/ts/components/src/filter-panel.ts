import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {CLUSTER_PREFIX, activeCount, artifactName, emptyDraft, isPopulated, withoutClause, withoutMember, type ClauseVerb, type ColumnDraft, type FilterDraft, type Layer, type MemberClause, type Meta, type Store} from '@tesseradb/client';
import {OPERATOR_WORDS} from './filter.js';
import {HeldAggregate} from './aggregate.js';
import {TesseraElement, UNNAMED, columnCaption, dateRangeText, emit, keyTitle, shortCount} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {countText, type TesseraFieldCard} from './field-card.js';
import {icon} from './icons.js';
import {exportparts, forwarded} from './parts.js';
import {FloatingList} from './float.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';
import './field-card.js';

/** Each card's parts, forwarded as `field-card-<part>`, and the parts of its search box as it forwards them. */
const CARD_PARTS = exportparts('field-card', [...forwarded('filter'), ...forwarded('cluster-filter')]);

/** The layers whose clusters a viewer can filter by in `view`: those `meta` lists for it that attach to no other. */
export function clusterFieldLayers(meta: Meta, view: string): Layer[] {
  return meta.layers.filter((l) => l.depsOn.length === 0 && (view === '' || l.views.includes(view)));
}

/** A layer's field as the panel keys it, apart from any column's. */
const layerField = (layer: string) => `${CLUSTER_PREFIX}${layer}`;

const VERBS: readonly ClauseVerb[] = ['filter', 'highlight'];

/** A figure in the subject row: whole, or shortened in the compact layout. */
const figure = (n: number, compact: boolean) => (compact ? shortCount(n) : countText(n));

/**
 * The field column: what the cards count, the clauses applied, and one `<tessera-field-card>` per
 * field.
 *
 * The subject row names what the cards' solid bars count, In view and the number of matching items
 * in the camera's box (the store's counts in view), or Highlighted and the number highlighted there
 * while a highlight is set, with a × that clears the highlight. Beside it, All matching names what
 * the pale bars count: every item the filters admit, from an aggregate the column keeps registered.
 *
 * While any clause is applied, a line lists each as a chip: a filter clause plain, a highlight
 * clause in the highlight colour, and a `member_of` clause by its cluster's name. Pressing a
 * column's chip opens and focuses its card, or under `chips-only` fires `tessera-chipopen`; its ×
 * empties that clause, leaving the other position's alone. Clear all empties every clause in both
 * positions and drops every `member_of` clause.
 *
 * The fields are the filterable columns and the layers whose clusters can be filtered by: every
 * layer `meta` lists for the current view that attaches to no other. The cards listed are the
 * fields in `pinned` (a layer's as `cluster:<layer>`), the fields holding a clause in either
 * position, the field the map is coloured by, and any field added, in `meta`'s order, the columns
 * then the layers; a field joins the end of the list, so the cards shown do not move. Add field lists every field, the columns then the layers, with a search box,
 * and the listed ones are checked and say Shown; its list opens under the column, as wide as it.
 * Choosing an unchecked field adds its card. Choosing a checked one takes the card off and empties
 * the field's clauses in both positions; a column in `pinned` stays, since the host lists it. Enter
 * in the search box adds the first match not yet listed and never takes one off. Beside Add field,
 * how many fields are not listed.
 *
 * A card folds to one line from its own button. Under `compact` every card is folded but one, and
 * opening one folds the one open before. `chips-only` renders the line of clauses alone, and
 * nothing while none is applied; `controls-only` leaves the subject row and the clauses out.
 *
 * @summary The field cards, what they count, and the clauses applied.
 * @tagname tessera-filter-panel
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-filterchange']>} tessera-filterchange - A
 *   column's chip was removed (with `verb`, the position it was in, and `expr` null), Clear all was
 *   pressed (with `column` and `expr` null), or the highlight was cleared from the subject row (with
 *   `column` and `expr` null and `verb` `highlight`). Removing a `member_of` chip fires nothing. Each
 *   card fires its own as well.
 * @fires {CustomEvent<TesseraEventDetails['tessera-chipopen']>} tessera-chipopen - A column's chip
 *   was pressed under `chips-only`, naming the column and the position of its clause.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - The words "View refused", with `data-code`, in the refused state.
 * @csspart subject - The subject row.
 * @csspart subject-key - The subject's colour, as its bars are drawn.
 * @csspart subject-name - In view, or Highlighted while a highlight is set.
 * @csspart subject-count - The number in view, or highlighted.
 * @csspart clear-highlight - The × beside Highlighted, which clears the highlight.
 * @csspart all - All matching and its number.
 * @csspart all-count - The number matching.
 * @csspart chips - The line of clauses applied, while there are any.
 * @csspart chip - One clause, with `data-verb` (`filter` or `highlight`) and `data-column` or
 *   `data-artifact`.
 * @csspart edit - The button that is a column chip's text, which opens its card.
 * @csspart clear - The Clear all button.
 * @csspart card - One `<tessera-field-card>`, with `data-field`.
 * @csspart add - The Add field button, with `aria-expanded`.
 * @csspart add-note - How many fields are not listed.
 * @csspart add-list - The list of fields to add, while it is open.
 * @csspart add-search - The search box over that list.
 * @csspart add-option - One field in that list, with `data-column` or `data-layer`, `aria-checked`
 *   while it is listed, and `aria-disabled` where it is pinned and so cannot be taken off.
 * @csspart field-card-<part> - A part of a card, forwarded under a `field-card-` prefix.
 * @csspart filter-<part> - A part of a card's `<tessera-filter>` search box.
 * @csspart cluster-filter-<part> - A part of a card's `<tessera-cluster-filter>` search box.
 */
export class TesseraFilterPanel extends TesseraElement {
  /** Renders the line of clauses alone, and nothing while none is applied. */
  @property({type: Boolean, attribute: 'chips-only'}) accessor chipsOnly = false;
  /** Renders the cards and Add field without the subject row and the clauses. */
  @property({type: Boolean, attribute: 'controls-only'}) accessor controlsOnly = false;
  /**
   * The columns whose cards are listed whether or not they hold a clause, space- or
   * comma-separated. Unset, only the fields holding a clause are listed until the user adds one.
   */
  @property() accessor pinned = '';
  /** Folds every card but one, and shortens the subject row's figures. */
  @property({type: Boolean, reflect: true}) accessor compact = false;
  /** The level the cards of levelled layers count at, passed to each; `null` is the deepest. */
  @property({type: Number, attribute: 'cluster-level'}) accessor clusterLevel: number | null = null;

  /** The fields the user added, which stay listed. @internal */
  @state() accessor opened: ReadonlySet<string> = new Set();
  /** The cards the user folded, outside the compact layout. @internal */
  @state() accessor folded: ReadonlySet<string> = new Set();
  /** The one card open in the compact layout. @internal */
  @state() accessor openCard: string | null = null;
  /** Whether the Add field list is open, and its search. @internal */
  @state() accessor adding = false;
  /** @internal */
  @state() accessor addSearch = '';
  /** The Add field option in the tab order, the one the arrow keys reached last. @internal */
  @state() accessor addActive = 0;

  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='subject'] {
        display: flex;
        align-items: center;
        flex-wrap: wrap;
        gap: 4px 14px;
        padding: 8px var(--_tessera-panel-inline, 14px);
        background: color-mix(in srgb, var(--_tessera-surface-2) 40%, var(--_tessera-surface));
        border-top: 1px solid var(--_tessera-line-2);
        border-bottom: 1px solid var(--_tessera-line-2);
        font-size: 12px;
        color: var(--_tessera-ink-2);
        font-variant-numeric: tabular-nums;
      }
      :host([compact]) [part='subject'] {
        gap: 4px 12px;
        padding: 6px var(--_tessera-panel-inline, 12px);
      }
      .key {
        display: flex;
        align-items: center;
        gap: 6px;
        white-space: nowrap;
      }
      :host([compact]) .key {
        gap: 5px;
      }
      .swatch {
        flex: none;
        width: 10px;
        height: 6px;
        border-radius: 1px;
        background: var(--_tessera-bar);
      }
      .swatch.lit {
        background: var(--_tessera-bar-highlight);
      }
      .swatch.pale {
        background: var(--_tessera-bar-match);
      }
      [part='subject-name'] {
        font-weight: 500;
        color: var(--_tessera-ink);
      }
      [part='subject-name'].lit {
        color: var(--_tessera-highlight);
      }
      [part='subject-count'] {
        font-weight: 600;
        color: var(--_tessera-ink);
      }
      [part='clear-highlight'] {
        width: 18px;
        height: 18px;
        display: grid;
        place-items: center;
        border-radius: 4px;
        color: var(--_tessera-ink-2);
      }
      [part='clear-highlight']:hover {
        background: var(--_tessera-surface-3);
      }
      [part='chips'] {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 6px;
        padding: 8px var(--_tessera-panel-inline, 14px);
        border-bottom: 1px solid var(--_tessera-line-2);
      }
      [part='chip'] {
        min-height: 0;
        padding: 2px 4px 2px 8px;
        gap: 4px;
      }
      [part='chip'] > button:last-child {
        width: 16px;
        height: 16px;
        display: grid;
        place-items: center;
      }
      .chip .edit {
        display: inline-flex;
        flex: 0 1 auto;
        align-items: center;
        color: inherit;
        font: inherit;
        text-align: left;
      }
      [part='clear'] {
        margin-left: auto;
        padding: 0;
        font-size: 12px;
        color: var(--_tessera-ink-2);
      }
      .adder {
        position: relative;
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 8px;
        padding: 10px var(--_tessera-panel-inline, 14px) 12px;
      }
      :host([compact]) .adder {
        padding: 8px var(--_tessera-panel-inline, 12px);
      }
      [part='add'] {
        height: auto;
        padding: 5px 10px;
      }
      :host([compact]) [part='add'] {
        padding: 4px 9px;
      }
      [part='add-note'] {
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
      /* The list opens over what sits below it, in the top layer. */
      [part='add-list'] {
        position: fixed;
        inset: auto;
        margin: 0;
        box-sizing: border-box;
        overflow-y: auto;
        background: var(--_tessera-surface);
        color: var(--_tessera-ink);
        font-size: 13px;
        padding: 4px;
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius-control);
        box-shadow: 0 6px 18px rgba(0, 0, 0, 0.08);
      }
      [part='add-list'] .input {
        margin: 2px 2px 6px;
      }
      [part~='add-option'] {
        display: flex;
        justify-content: space-between;
        gap: 8px;
        width: 100%;
        padding: 6px 8px;
        border-radius: 4px;
        text-align: left;
      }
      [part~='add-option']:hover,
      [part~='add-option']:focus-visible {
        background: var(--_tessera-surface-2);
      }
      [part~='add-option']:focus-visible {
        outline-offset: -2px;
      }
      [part~='add-option'][aria-disabled='true'] {
        cursor: default;
      }
      [part~='add-option'] .kind {
        font-size: 12px;
        font-weight: 400;
        color: var(--_tessera-ink-3);
      }
      .none {
        display: block;
        padding: 6px 8px;
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
    `
  ];

  /** Every item the filters admit, which All matching counts. */
  private readonly matching = new HeldAggregate('fields-match');
  /** The card to open, scroll to and focus once the cards are drawn. */
  private editing: string | null = null;
  /** The keys of the fields shown, in the order they were first shown. */
  private listOrder: string[] = [];
  private readonly floating = new FloatingList(() => {
    const list = this.renderRoot.querySelector<HTMLElement>('[part="add-list"]');
    const anchor = this.renderRoot.querySelector<HTMLElement>('.adder');
    return list && anchor ? {list, anchor} : null;
  });

  /** What the column last drew from, so a publish that changes none of it draws nothing. */
  private drawnFrom: readonly unknown[] = [];

  /**
   * Draw again only when what the column reads changed: the clauses, the counts, the colouring, the
   * view and the clusters served, whose names a chip may carry. The store publishes as each frame
   * arrives, which changes none of these.
   */
  protected override onStoreChange(): void {
    const s = this.resolvedStore;
    const view = s?.get('view');
    const now = s && view ? [s.get('filters'), s.get('aggregates'), s.get('legend').colourBy, s.get('meta'), s.get('status').status, view.id, view.inView?.matched.value, view.inView?.highlighted.value, s.get('artifacts').served] : [];
    if (now.length === this.drawnFrom.length && now.every((v, i) => v === this.drawnFrom[i])) return;
    this.drawnFrom = now;
    super.onStoreChange();
  }

  protected override onStoreAdopted(): void {
    this.drawnFrom = [];
  }

  override disconnectedCallback(): void {
    this.floating.stop();
    this.matching.set(null, null);
    super.disconnectedCallback();
  }

  /** List `field`'s card, open it, and scroll to and focus it once drawn. A host showing the cards after `tessera-chipopen` calls this. */
  show(field: string): void {
    this.opened = new Set([...this.opened, field]);
    this.unfold(field);
    this.editing = field;
    this.requestUpdate();
  }

  /** Moves focus to the first card's search box or plot, else Add field. */
  override focus(options?: FocusOptions): void {
    const card = this.renderRoot.querySelector<HTMLElement>('[part~="card"]');
    const target = card?.shadowRoot?.querySelector<HTMLElement>('tessera-filter, tessera-cluster-filter, [part="plot"], [part="fold"]') ?? this.renderRoot.querySelector<HTMLElement>('[part="add"]');
    if (target) target.focus(options);
    else super.focus(options);
  }

  private unfold(field: string): void {
    if (this.compact) this.openCard = field;
    else if (this.folded.has(field)) {
      const next = new Set(this.folded);
      next.delete(field);
      this.folded = next;
    }
  }

  /** A card's own fold button: in the compact layout, opening one folds the one open before. */
  private onToggle(field: string, card: TesseraFieldCard): void {
    if (this.compact) {
      this.openCard = card.folded ? null : field;
      return;
    }
    const next = new Set(this.folded);
    if (card.folded) next.add(field);
    else next.delete(field);
    this.folded = next;
  }

  /** A card the user has changed stays listed, even as its clause empties under them. */
  private keepListed(field: string): void {
    if (!this.opened.has(field)) this.opened = new Set([...this.opened, field]);
  }

  private edit(column: string, verb: ClauseVerb): void {
    if (this.chipsOnly) emit(this, 'tessera-chipopen', {column, verb});
    else this.show(column);
  }

  protected override updated(changed: PropertyValues<this>): void {
    super.updated(changed);
    this.floating.update();
    const s = this.resolvedStore;
    this.matching.set(s, this.isConnected && !this.chipsOnly && !this.controlsOnly && s?.get('meta') ? {groupings: [{}]} : null);
    if (!this.editing || this.chipsOnly) return;
    const card = Array.from(this.renderRoot.querySelectorAll<TesseraFieldCard>('[part~="card"]')).find((c) => c.field === this.editing);
    if (!card) return;
    this.editing = null;
    void card.updateComplete.then(() => {
      card.scrollIntoView({block: 'nearest'});
      (card.shadowRoot?.querySelector<HTMLElement>('tessera-filter, tessera-cluster-filter, [part="plot"]') ?? card.shadowRoot?.querySelector<HTMLElement>('[part="fold"]'))?.focus();
    });
  }

  private chipText(column: string, draft: ColumnDraft): string {
    const caption = columnCaption(column);
    switch (draft.family) {
      case 'text':
        return `${caption}: ${draft.expr !== undefined ? JSON.stringify(draft.expr) : draft.query.trim().replace(/"([^"]*)"?/g, '“$1”')}`;
      case 'keyword':
        return `${caption} ${OPERATOR_WORDS[draft.op]} ${draft.needle}`;
      case 'category':
        return `${caption}: ${draft.keys.map((k) => keyTitle(this.resolvedStore, column, k)).join(', ')}`;
      case 'numeric': {
        const meta = this.resolvedStore?.get('meta');
        if (meta?.declaredScalars.find((c) => c.name === column)?.arrowType === 'timestamp_us') return `${caption} ${dateRangeText(draft.gte, draft.lte)}`;
        const f = (v: number) => v.toLocaleString('en-GB');
        if (draft.lte === null) return `${caption} ≥ ${f(draft.gte!)}`;
        if (draft.gte === null) return `${caption} ≤ ${f(draft.lte)}`;
        return `${caption} ${f(draft.gte)} – ${f(draft.lte)}`;
      }
    }
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

  private clearMember(clause: MemberClause): void {
    const s = this.resolvedStore;
    if (!s) return;
    s.setMembers(withoutMember(s.get('filters').members, clause.layer, clause.artifact, clause.verb));
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

  /** Empty every clause in the highlight position, so the cards count what is in view again. */
  private clearHighlight(): void {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    if (!s || !meta) return;
    const {draft, members} = s.get('filters');
    s.setFilters({...draft, highlight: emptyDraft(meta.filterOperands).highlight});
    if (members.some((m) => m.verb === 'highlight')) s.setMembers(members.filter((m) => m.verb !== 'highlight'));
    emit(this, 'tessera-filterchange', {column: null, verb: 'highlight', expr: null});
  }

  private add(field: string): void {
    this.adding = false;
    this.addSearch = '';
    this.show(field);
  }

  /** Take a field off the column, emptying its clauses in both positions. */
  private takeOff(field: string): void {
    this.adding = false;
    this.addSearch = '';
    const opened = new Set(this.opened);
    opened.delete(field);
    this.opened = opened;
    const s = this.resolvedStore;
    if (s && field.startsWith(CLUSTER_PREFIX)) {
      const layer = field.slice(CLUSTER_PREFIX.length);
      const members = s.get('filters').members;
      if (members.some((m) => m.layer === layer)) s.setMembers(members.filter((m) => m.layer !== layer));
    } else if (s) {
      const draft = s.get('filters').draft;
      const held = VERBS.filter((verb) => {
        const d = draft[verb][field];
        return d !== undefined && isPopulated(d);
      });
      if (held.length > 0) {
        s.setFilters(held.reduce((d, verb) => withoutClause(d, field, verb), draft));
        for (const verb of held) emit(this, 'tessera-filterchange', {column: field, verb, expr: null});
      }
    }
    void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="add"]')?.focus());
  }

  /** The arrow keys, Home and End move among the Add field options; Up from the first goes to the search box. */
  private addKeys(e: KeyboardEvent, i: number | null): void {
    const items = Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part~="add-option"]'));
    if (items.length === 0) return;
    const last = items.length - 1;
    const next = i === null ? {ArrowDown: 0}[e.key] : {ArrowDown: Math.min(i + 1, last), ArrowUp: i - 1, Home: 0, End: last}[e.key];
    if (next === undefined) return;
    e.preventDefault();
    if (next < 0) {
      this.renderRoot.querySelector<HTMLElement>('[part="add-search"]')?.focus();
      return;
    }
    this.addActive = next;
    items[next]?.focus();
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel">${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const operands = meta.filterOperands;
    const layers = clusterFieldLayers(meta, s.get('view').id);
    if (operands.length === 0 && layers.length === 0) return html`<div class="panel"><span part="state" data-state="empty">Nothing to filter</span></div>`;
    const {draft, members} = s.get('filters');
    const holds = (column: string, verb: ClauseVerb) => {
      const d = draft[verb][column];
      return d !== undefined && isPopulated(d);
    };
    const chips = this.chipLine(draft, members, holds);
    if (this.chipsOnly) return chips;

    const fields = [
      ...operands.map((o) => ({key: o.column, title: columnCaption(o.column), layer: null as Layer | null})),
      ...layers.map((l) => ({key: layerField(l.name), title: l.title || l.name, layer: l as Layer | null}))
    ];
    const pinned = new Set(this.pinned.split(/[\s,]+/).filter(Boolean));
    const holdsField = (f: (typeof fields)[number], verb: ClauseVerb) => (f.layer ? members.some((m) => m.layer === f.layer!.name && m.verb === verb) : holds(f.key, verb));
    // The field the map is coloured by has a card, which says what its colours mean.
    const colourBy = s.get('legend').colourBy;
    const shown = fields.filter((f) => pinned.has(f.key) || this.opened.has(f.key) || f.key === colourBy || holdsField(f, 'filter') || holdsField(f, 'highlight'));
    // A field joins the end of the list, so the cards already shown do not move.
    const keys = new Set(shown.map((f) => f.key));
    this.listOrder = [...this.listOrder.filter((k) => keys.has(k)), ...shown.map((f) => f.key).filter((k) => !this.listOrder.includes(k))];
    const listed = this.listOrder.map((k) => shown.find((f) => f.key === k)!);
    const open = this.compact ? (listed.some((f) => f.key === this.openCard) ? this.openCard : null) : null;
    const card = (f: (typeof fields)[number]) =>
      html`<tessera-field-card part="card" exportparts=${CARD_PARTS} data-field=${f.key} field=${f.key} .store=${s} .level=${this.clusterLevel}
        ?compact=${this.compact} .folded=${this.compact ? f.key !== open : this.folded.has(f.key)}
        @tessera-fold=${(e: Event) => this.onToggle(f.key, e.currentTarget as TesseraFieldCard)}
        @tessera-filterchange=${() => this.keepListed(f.key)} @tessera-clausechange=${() => this.keepListed(f.key)}></tessera-field-card>`;
    const head = this.controlsOnly ? nothing : html`${this.subjectRow(s)}${chips}`;
    return html`${head}<span part="state" data-state="shown"></span>${repeat(listed, (f) => f.key, card)}${this.adder(fields, listed, pinned)}`;
  }

  /** In view or Highlighted and its number, then All matching and its number. */
  private subjectRow(s: Store): TemplateResult {
    const {draft, members} = s.get('filters');
    const lit = activeCount(draft, 'highlight') > 0 || members.some((m) => m.verb === 'highlight');
    const inView = s.get('view').inView;
    const n = inView ? (lit ? inView.highlighted.value : inView.matched.value) : null;
    const total = this.matching.entry()?.result?.tables[0]?.total ?? null;
    const skel = html`<span class="skel" aria-hidden="true"></span>`;
    return html`<div part="subject">
      <span class="key"><span part="subject-key" class=${`swatch${lit ? ' lit' : ''}`}></span><span part="subject-name" class=${lit ? 'lit' : ''}>${lit ? 'Highlighted' : 'In view'}</span><span part="subject-count">${n === null ? skel : figure(n, this.compact)}</span>${lit
        ? html`<button part="clear-highlight" type="button" aria-label="Clear highlight" title="Clear highlight" @click=${() => this.clearHighlight()}>${icon('close', 11, 2.4)}</button>`
        : nothing}</span>
      <span part="all" class="key"><span class="swatch pale"></span><span>${this.compact ? 'All' : 'All matching'}</span><span part="all-count">${total === null ? skel : figure(total, this.compact)}</span></span>
    </div>`;
  }

  /** Every clause as a chip, then Clear all; nothing while none is applied. */
  private chipLine(draft: FilterDraft, members: readonly MemberClause[], holds: (column: string, verb: ClauseVerb) => boolean): TemplateResult | typeof nothing {
    const columns = [...new Set([...Object.keys(draft.filter), ...Object.keys(draft.highlight)])];
    const chips = VERBS.flatMap((verb) => columns.flatMap((c) => (holds(c, verb) ? [{c, verb, d: draft[verb][c]!}] : [])));
    if (chips.length === 0 && members.length === 0) return nothing;
    return html`<div part="chips">
      ${chips.map(
        ({c, verb, d}) => html`<span part="chip" class="chip" data-verb=${verb} data-column=${c}
          ><button part="edit" class="edit" type="button" title="Show its card" @click=${() => this.edit(c, verb)}>${this.chipText(c, d)}</button
          ><button type="button" aria-label=${`Remove the ${columnCaption(c)} ${verb}`} @click=${() => this.clearColumn(c, verb)}>${icon('close', 10, 2.4)}</button></span
        >`
      )}
      ${[...members].sort((a, b) => VERBS.indexOf(a.verb) - VERBS.indexOf(b.verb)).map(
        (m) => html`<span part="chip" class="chip" data-verb=${m.verb} data-artifact=${String(m.artifact)}
          >${this.memberText(m)}<button type="button" aria-label=${`Remove ${this.memberText(m)}`} @click=${() => this.clearMember(m)}>${icon('close', 10, 2.4)}</button></span
        >`
      )}
      <button part="clear" type="button" @click=${() => this.clearAll()}>Clear all</button>
    </div>`;
  }

  /** Add field, its list of every field, and how many are not listed. */
  private adder(fields: {key: string; title: string; layer: Layer | null}[], listed: {key: string}[], pinned: Set<string>): TemplateResult {
    const q = this.addSearch.trim().toLowerCase();
    const offered = fields.filter((f) => q === '' || f.key.toLowerCase().includes(q) || f.title.toLowerCase().includes(q));
    const tabbed = this.addActive < offered.length ? this.addActive : 0;
    const isListed = (key: string) => listed.some((f) => f.key === key);
    const choose = (key: string) => {
      if (!isListed(key)) this.add(key);
      else if (!pinned.has(key)) this.takeOff(key);
    };
    const rest = fields.length - listed.length;
    return html`<div class="adder">
      <button part="add" class="btn" type="button" aria-expanded=${this.adding ? 'true' : 'false'} aria-controls="add-list"
        @click=${() => {
          this.adding = !this.adding;
          this.addSearch = '';
          this.addActive = 0;
        }}>${icon('plus', 12, 1.6)}Add field</button>
      ${rest > 0 && !this.compact ? html`<span part="add-note">${rest} more ${rest === 1 ? 'field' : 'fields'}</span>` : nothing}
      ${this.adding
        ? html`<div part="add-list" id="add-list" popover="manual" @keydown=${(e: KeyboardEvent) => {
            if (e.key !== 'Escape') return;
            e.stopPropagation();
            this.adding = false;
            this.renderRoot.querySelector<HTMLElement>('[part="add"]')?.focus();
          }}>
            <div class="input">${icon('search', 14)}<input part="add-search" type="search" autocomplete="off" placeholder="Find a field" aria-label="Find a field" .value=${this.addSearch}
              @input=${(e: Event) => {
                this.addSearch = (e.target as HTMLInputElement).value;
                this.addActive = 0;
              }}
              @keydown=${(e: KeyboardEvent) => {
                // Enter adds the first match not yet listed. Taking a field off is always a press on its row.
                if (e.key !== 'Enter') return this.addKeys(e, null);
                e.preventDefault();
                const unlisted = offered.find((o) => !isListed(o.key));
                if (unlisted) this.add(unlisted.key);
              }} /></div>
            ${offered.length > 0
              ? html`<div role="menu" aria-label="Fields">${offered.map((o, i) => {
                  const added = isListed(o.key);
                  const kept = added && pinned.has(o.key);
                  return html`<button part="add-option" type="button" role="menuitemcheckbox" data-column=${o.layer ? nothing : o.key} data-layer=${o.layer ? o.layer.name : nothing}
                    aria-checked=${added ? 'true' : 'false'}
                    aria-disabled=${kept ? 'true' : nothing} title=${kept ? 'Always shown here' : added ? 'Remove from the column' : nothing} tabindex=${i === tabbed ? '0' : '-1'}
                    @click=${() => choose(o.key)} @keydown=${(e: KeyboardEvent) => this.addKeys(e, i)}><span>${o.title}</span>${added ? html`<span class="kind">Shown</span>` : o.layer ? html`<span class="kind">Clusters</span>` : nothing}</button>`;
                })}</div>`
              : html`<span class="none">No field by that name</span>`}
          </div>`
        : nothing}
    </div>`;
  }

  protected override willUpdate(changed: PropertyValues<this>): void {
    super.willUpdate(changed);
    // The search box takes focus as the list opens.
    if (changed.has('adding') && this.adding) void this.updateComplete.then(() => this.renderRoot.querySelector<HTMLElement>('[part="add-search"]')?.focus());
  }
}

attachContextRoot();
defineOnce('tessera-filter-panel', TesseraFilterPanel);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-filter-panel': TesseraFilterPanel;
  }
}
