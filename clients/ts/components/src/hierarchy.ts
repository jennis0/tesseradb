import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {browsableLayers, isFilterLayer, refusalOf, withMember, withoutMember, type BrowsePage, type BrowseRow, type ClauseVerb, type Layer, type Masked, type Refusal} from '@tesseradb/client';
import {TesseraElement, UNNAMED, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';


/** One node of the walk: a row, its children once fetched, and where its paging got to. */
type Node = {
  row: BrowseRow;
  /** The path that reached it; a `dag` node under two parents is two nodes with different paths. */
  path: string;
  children: Node[] | null;
  next: string | null;
  loading: boolean;
  refusal: Refusal | null;
};

const masked = (n: bigint): Masked => ({value: Number(n), exact: true});

/**
 * A layer's hierarchy, browsed through `POST /v1/artifacts/browse` independently of the viewport:
 * it opens on the roots at any zoom and does not move with the map. A select chooses among the
 * bundle's hierarchical layers where there are several. Each row is a name, a masked count and an
 * expander; expanding fetches the children, a page at a time under More. On a layer where a child
 * may have several parents, a row appears under each parent it is served under and names the
 * others. A parent the viewer may not see is absent, so its child reads as a root. The search box
 * lists matching names.
 *
 * The panel sends the map's filters and, while any is set, shows each row's matched count beside
 * its masked count. A row's presence and its masked count do not change with the filters. A row's
 * buttons put a `member_of` clause on its artifact as a highlight or a filter, and Fit fits the map
 * to it on a layer that draws. Pressing the name highlights it.
 *
 * The panel asks for nothing while it is hidden. What is expanded and paged is the element's own
 * state, not the store's.
 *
 * @summary A layer's hierarchy, browsed apart from the viewport.
 * @tagname tessera-hierarchy
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-clausechange']>} tessera-clausechange - A row's
 *   name, highlight or filter button put a `member_of` clause on or took it off.
 * @fires {CustomEvent<TesseraEventDetails['tessera-artifactfit']>} tessera-artifactfit - A row's Fit
 *   button was pressed.
 * @csspart title - The heading.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - A refusal's code and detail.
 * @csspart layer - The layer select, shown where there are several layers.
 * @csspart search - The search box.
 * @csspart tree - The tree of rows.
 * @csspart row - One row, with `data-id`, and `data-clause` (`filter` or `highlight`) while a
 *   clause is on its artifact.
 * @csspart expander - A row's expand button.
 * @csspart name - A row's name, which highlights the artifact when pressed.
 * @csspart counts - A row's counts.
 * @csspart count-matched - A row's matched `<tessera-count>`, while a filter is set.
 * @csspart count-masked - A row's masked `<tessera-count>`.
 * @csspart actions - A row's buttons.
 * @csspart highlight - A row's highlight button, with `aria-pressed`.
 * @csspart filter - A row's filter button, with `aria-pressed`.
 * @csspart fit - A row's Fit button, on a layer that draws.
 * @csspart also - The other parents a row is served under, or a refusal of its children.
 * @csspart children - An expanded row's children.
 * @csspart more - The More button that fetches the next page.
 */
export class TesseraHierarchy extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='layer'] {
        width: 100%;
        margin-bottom: 8px;
      }
      [part='search'] {
        margin-bottom: 8px;
      }
      [part='tree'] {
        list-style: none;
        margin: 0;
        padding: 0;
        max-height: 320px;
        overflow-y: auto;
      }
      [part='row'] {
        display: flex;
        align-items: center;
        gap: 6px;
        min-height: 28px;
        padding: 0 4px 0 calc(2px + var(--depth, 0) * 14px);
        border-radius: 3px;
      }
      [part='row']:hover {
        background: var(--_tessera-surface-2);
      }
      [part='row'][data-clause='highlight'] {
        background: var(--_tessera-highlight-soft);
        color: var(--_tessera-highlight);
      }
      [part='row'][data-clause='filter'] {
        background: var(--_tessera-accent-soft);
        color: var(--_tessera-accent);
      }
      [part='expander'] {
        display: inline-flex;
        width: 14px;
        color: var(--_tessera-ink-3);
      }
      [part='expander'][data-leaf] {
        visibility: hidden;
      }
      [part='name'] {
        flex: 1;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
        text-align: left;
      }
      [part='name'][data-unnamed] {
        color: var(--_tessera-ink-2);
      }
      [part='counts'] {
        display: inline-flex;
        gap: 6px;
        align-items: baseline;
        font-size: 12px;
        color: var(--_tessera-ink-2);
      }
      [part='actions'] {
        display: inline-flex;
        gap: 2px;
        opacity: 0;
      }
      [part='row']:hover [part='actions'],
      [part='row']:focus-within [part='actions'] {
        opacity: 1;
      }
      [part='actions'] button {
        display: inline-flex;
        padding: 2px;
        color: var(--_tessera-ink-3);
        border-radius: 2px;
      }
      [part='actions'] button:hover {
        color: var(--_tessera-ink);
        background: var(--_tessera-surface-3);
      }
      [part='also'] {
        padding-left: calc(18px + var(--depth, 0) * 14px);
        font-size: 11px;
        color: var(--_tessera-ink-3);
      }
      [part='more'] {
        padding-left: calc(18px + var(--depth, 0) * 14px);
        font-size: 12px;
        color: var(--_tessera-accent);
      }
      [part='lineage'] {
        font-size: 11px;
        color: var(--_tessera-ink-3);
      }
    `
  ];

  /** The layer whose hierarchy is shown. Unset or unknown, the first the bundle offers. */
  @property() accessor layer = '';
  /** Rows per page, at least 1 and at most the server's `maxBrowseRows`. */
  @property({type: Number}) accessor limit = 50;

  @state() private accessor roots: Node[] | null = null;
  @state() private accessor rootsNext: string | null = null;
  @state() private accessor query = '';
  @state() private accessor searching: Node[] | null = null;
  @state() private accessor loading = false;
  @state() private accessor refusal: Refusal | null = null;
  /** Which paths are open, so a re-render of the tree keeps the walk. */
  @state() private accessor open = new Set<string>();
  /** Bumped to re-render after a node's children land, since the tree in `roots` is mutated. */
  @state() private accessor revision = 0;

  /** The question the roots were fetched under; see {@link question}. */
  private fetchedUnder = '';
  /**
   * Every name the walk has seen, by identifier. A filter layer's artifacts are never served in a
   * viewport, so this is the client's only source of their names.
   */
  private names = new Map<bigint, string>();
  private searchTimer: ReturnType<typeof setTimeout> | null = null;

  private layers(): Layer[] {
    return browsableLayers(this.resolvedStore?.get('meta')?.layers ?? []);
  }

  private current(): Layer | null {
    const offered = this.layers();
    return offered.find((l) => l.name === this.layer) ?? offered[0] ?? null;
  }

  /**
   * The question the panel is asking, as a string: the layer and the map's filter. A change
   * refetches the roots and drops the walk, whose counts answer the old question.
   */
  private question(): string {
    // Hashed from `requestFilters()`, what `store.browse` sends, which includes the drawn region.
    // A `member_of` leaf holds its artifact as a decimal string, so it serialises.
    return `${this.current()?.name ?? ''}|${JSON.stringify(this.resolvedStore?.requestFilters() ?? null)}`;
  }

  /**
   * Whether the panel is shown. A hidden panel sends no request, so a panel in a closed drawer
   * does not load the roots alongside the first viewport.
   *
   * `checkVisibility` is asked on every render; a reveal that re-renders nothing (a drawer opening
   * above it) is caught by the intersection observer. A runtime with neither counts as shown.
   */
  @state() private accessor shown = false;
  private observer: IntersectionObserver | null = null;

  override connectedCallback(): void {
    super.connectedCallback();
    if (typeof IntersectionObserver === 'undefined') return;
    this.observer ??= new IntersectionObserver((entries) => {
      if (!entries.some((e) => e.isIntersecting) || this.shown) return;
      this.shown = true;
      this.loadRootsIfStale();
    });
    this.observer.observe(this);
  }

  override disconnectedCallback(): void {
    this.observer?.disconnect();
    this.observer = null;
    super.disconnectedCallback();
  }

  /** Whether this element is displayed; see {@link shown}. */
  private displayed(): boolean {
    return typeof this.checkVisibility === 'function' ? this.checkVisibility() : true;
  }

  /** The roots, where the question they were fetched under is no longer the one being asked. */
  private loadRootsIfStale(): void {
    const q = this.question();
    if (this.shown && q !== this.fetchedUnder && this.current()) {
      this.fetchedUnder = q;
      void this.loadRoots();
    }
  }

  protected override onStoreChange(): void {
    this.loadRootsIfStale();
    super.onStoreChange();
  }

  /** Re-asked before every render, so putting the panel away and taking it out again both count. */
  protected override willUpdate(changed: PropertyValues<this>): void {
    super.willUpdate(changed);
    if (!this.shown && this.displayed()) this.shown = true;
  }

  protected override updated(): void {
    if (this.shown && this.roots === null && this.current() && this.resolvedStore && !this.loading && this.fetchedUnder === '') {
      this.fetchedUnder = this.question();
      void this.loadRoots();
    }
  }

  /** One page of the current layer, under the map's filter, which the store supplies. */
  private page(req: {parent?: bigint; q?: string; cursor?: string}): Promise<BrowsePage> | null {
    const s = this.resolvedStore;
    const layer = this.current();
    if (!s || !layer) return null;
    const ceiling = s.get('meta')?.selection.maxBrowseRows ?? this.limit;
    return s.browse({...req, layer: layer.name, limit: Math.max(1, Math.min(this.limit, ceiling))});
  }

  private async loadRoots(cursor?: string): Promise<void> {
    const request = this.page(cursor === undefined ? {} : {cursor});
    if (!request) return;
    this.loading = true;
    this.refusal = null;
    try {
      const page = await request;
      const nodes = page.artifacts.map((row) => this.node(row, ''));
      this.roots = cursor === undefined ? nodes : [...(this.roots ?? []), ...nodes];
      this.rootsNext = page.next;
    } catch (error) {
      this.refusal = refusalOf(error);
      this.roots = this.roots ?? [];
    } finally {
      this.loading = false;
    }
  }

  private node(row: BrowseRow, parentPath: string): Node {
    if (row.name !== null) this.names.set(row.tesseraId, row.name);
    return {row, path: `${parentPath}/${row.tesseraId}`, children: null, next: null, loading: false, refusal: null};
  }

  /** What to call an artifact: its name where the walk has met one. */
  private nameOf(id: bigint): string {
    return this.names.get(id) ?? UNNAMED;
  }

  /**
   * Expand a node: its children, paged. A node keeps what it fetched while {@link question} is
   * unchanged, so reopening it fetches nothing.
   */
  private async expand(node: Node, more = false): Promise<void> {
    const request = this.page({parent: node.row.tesseraId, ...(more && node.next ? {cursor: node.next} : {})});
    if (!request) return;
    node.loading = true;
    node.refusal = null;
    this.revision++;
    try {
      const page = await request;
      const nodes = page.artifacts.map((row) => this.node(row, node.path));
      node.children = more ? [...(node.children ?? []), ...nodes] : nodes;
      node.next = page.next;
    } catch (error) {
      node.refusal = refusalOf(error);
      node.children = node.children ?? [];
    } finally {
      node.loading = false;
      this.revision++;
    }
  }

  private toggle(node: Node): void {
    const open = new Set(this.open);
    if (open.has(node.path)) open.delete(node.path);
    else {
      open.add(node.path);
      if (node.children === null && !node.loading) void this.expand(node);
    }
    this.open = open;
  }

  private search(text: string): void {
    this.query = text;
    if (this.searchTimer) clearTimeout(this.searchTimer);
    // Debounced, like every typed control.
    this.searchTimer = setTimeout(() => {
      this.searchTimer = null;
      const q = text.trim();
      if (q.length === 0) {
        this.searching = null;
        return;
      }
      const request = this.page({q});
      if (!request) return;
      void request
        .then((page) => {
          this.searching = page.artifacts.map((row) => this.node(row, 'q'));
        })
        .catch((error: unknown) => {
          this.refusal = refusalOf(error);
          this.searching = [];
        });
    }, 250);
  }

  /** The clause on this row's artifact, if any, which colours the row and sets the buttons. */
  private clauseOn(id: bigint): ClauseVerb | null {
    const layer = this.current()?.name;
    const held = this.resolvedStore?.get('filters').members ?? [];
    return held.find((c) => c.layer === layer && c.artifact === id && !c.outside)?.verb ?? null;
  }

  private apply(id: bigint, verb: ClauseVerb): void {
    const s = this.resolvedStore;
    const layer = this.current();
    if (!s || !layer) return;
    const held = s.get('filters').members;
    const on = this.clauseOn(id) === verb;
    const label = this.names.get(id);
    s.setMembers(
      on
        ? withoutMember(held, layer.name, id)
        : withMember(held, {layer: layer.name, artifact: id, outside: false, verb, ...(label === undefined ? {} : {label})})
    );
    emit(this, 'tessera-clausechange', {id: idString(id), layer: layer.name, outside: false, verb, on: !on});
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const heading = html`<h2 part="title">Hierarchy</h2>`;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel">${heading}${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const offered = this.layers();
    if (offered.length === 0) return html`<div class="panel">${heading}<span part="state" data-state="empty">No hierarchical layers</span></div>`;
    const layer = this.current()!;
    const filtered = s.get('filters').expr !== null || s.get('filters').members.length > 0;
    const rows = this.searching ?? this.roots;
    return html`<div class="panel">${heading}
      <span part="state" data-state=${this.refusal ? 'refused' : this.loading && rows === null ? 'loading' : 'shown'}></span>
      ${offered.length > 1
        ? html`<select part="layer" aria-label="Hierarchy layer" .value=${layer.name} @change=${(e: Event) => {
            this.layer = (e.target as HTMLSelectElement).value;
            this.roots = null;
            this.searching = null;
            this.open = new Set();
          }}>
            ${offered.map((l) => html`<option value=${l.name} ?selected=${l.name === layer.name}>${l.title || l.name}${isFilterLayer(l) ? ' (filter layer)' : ''}</option>`)}
          </select>`
        : nothing}
      <div part="search" class="input">${icon('search', 14)}<input
        type="search"
        aria-label=${`Search ${layer.name}`}
        .value=${this.query}
        placeholder="Search names"
        autocomplete="off"
        @input=${(e: Event) => this.search((e.target as HTMLInputElement).value)}
      /></div>
      ${this.refusal ? html`<span part="refusal">${this.refusal.code}: ${this.refusal.detail}</span>` : nothing}
      ${rows === null
        ? nothing
        : rows.length === 0
          ? html`<span part="state" data-state="empty">${this.searching ? 'No match' : 'Nothing here'}</span>`
          : html`<ul part="tree">${rows.map((n) => this.renderNode(n, 0, layer, filtered))}</ul>`}
      ${this.searching === null && this.rootsNext
        ? html`<button part="more" type="button" @click=${() => void this.loadRoots(this.rootsNext ?? undefined)}>More…</button>`
        : nothing}
    </div>`;
  }

  private renderNode(node: Node, depth: number, layer: Layer, filtered: boolean): unknown {
    const open = this.open.has(node.path);
    const clause = this.clauseOn(node.row.tesseraId);
    const name = node.row.name;
    // A `dag` node is drawn under each served parent; the row names the others.
    const also = node.row.parentIds.filter((p) => String(p) !== node.path.split('/').at(-2));
    const drawn = !isFilterLayer(layer);
    return html`<li>
      <div part="row" style=${`--depth:${depth}`} data-id=${idString(node.row.tesseraId)} data-clause=${clause ?? nothing}>
        <button part="expander" type="button" data-leaf=${layer.hierarchy.kind === 'flat' ? '' : nothing}
          aria-expanded=${open ? 'true' : 'false'}
          aria-label=${open ? `Collapse ${name ?? 'row'}` : `Expand ${name ?? 'row'}`}
          @click=${() => this.toggle(node)}>${icon(open ? 'chev' : 'chevr', 12)}</button>
        <button part="name" type="button" data-unnamed=${name === null ? '' : nothing}
          title="Highlight this: the map stays and its members are lit"
          @click=${() => this.apply(node.row.tesseraId, 'highlight')}>${name ?? UNNAMED}</button>
        <span part="counts">
          ${filtered && node.row.matchedCount !== null
            ? html`<tessera-count part="count-matched" .masked=${masked(node.row.matchedCount)}></tessera-count>/`
            : nothing}<tessera-count part="count-masked" .masked=${masked(node.row.maskedCount)}></tessera-count>
        </span>
        <span part="actions">
          <button part="highlight" type="button" data-verb="highlight" aria-pressed=${clause === 'highlight' ? 'true' : 'false'}
            title="Highlight this" @click=${() => this.apply(node.row.tesseraId, 'highlight')}>${icon('highlight', 13)}</button>
          <button part="filter" type="button" data-verb="filter" aria-pressed=${clause === 'filter' ? 'true' : 'false'}
            title="Filter to this" @click=${() => this.apply(node.row.tesseraId, 'filter')}>${icon('filter', 13)}</button>
          ${
            // A filter layer draws nothing, so there is nothing to fit to.
            drawn
              ? html`<button part="fit" type="button" title="Fit the map to this"
                  @click=${() => emit(this, 'tessera-artifactfit', {id: idString(node.row.tesseraId)})}>${icon('fit', 13)}</button>`
              : nothing
          }
        </span>
      </div>
      ${also.length > 0 ? html`<div part="also" style=${`--depth:${depth}`}>also under ${also.map((p) => this.nameOf(p)).join(', ')}</div>` : nothing}
      ${node.refusal ? html`<div part="also" style=${`--depth:${depth}`}>${node.refusal.code}: ${node.refusal.detail}</div>` : nothing}
      ${open && node.children
        ? html`<ul part="children" class="list" style="list-style:none;margin:0;padding:0">
            ${node.children.map((c) => this.renderNode(c, depth + 1, layer, filtered))}
            ${node.next
              ? html`<li><button part="more" style=${`--depth:${depth + 1}`} type="button" @click=${() => void this.expand(node, true)}>More…</button></li>`
              : nothing}
          </ul>`
        : nothing}
    </li>`;
  }
}

attachContextRoot();
defineOnce('tessera-hierarchy', TesseraHierarchy);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-hierarchy': TesseraHierarchy;
  }
}
