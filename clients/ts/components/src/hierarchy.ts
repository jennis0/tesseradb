import {css, html, nothing} from 'lit';
import {property, state} from 'lit/decorators.js';
import {browsableLayers, isFilterLayer, withMember, withoutMember, type BrowsePage, type BrowseRow, type ClauseVerb, type Layer, type Masked, type Refusal} from '@tesseradb/client';
import {TesseraElement, UNNAMED, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * `<tessera-hierarchy>` — a layer's hierarchy, browsable **independently of the viewport**
 * (`highlight-and-hierarchy.md` §5.1), over `POST /v1/artifacts/browse` (§4).
 *
 * Rung 3 is why it exists: a 30,217-descriptor DAG went into a bundle and the viewer could show
 * none of it. The viewport serves a budget cut — roughly 48 artifacts at zoom 0, doubling per
 * level — and a MeSH descriptor's members are spread over the whole layout, so the cut deepens
 * uniformly and the leaves arrive at a zoom nobody reaches. **Nothing here depends on the
 * viewport**: it opens on the roots whatever the zoom, and it does not move when the map does.
 *
 * A layer picker over the bundle's hierarchical layers, then a tree. Each row is a name, a masked
 * count and an expander; a child fetches its children on expansion and pages under *N more*. On a
 * `dag` layer a node appears under **each** of its served parents and says *also under X* by
 * listing its other parents — C29 per entry, so a parent this principal may not see is simply
 * absent and the node reads as a root of what they were given. A search box drives §4's search
 * form and shows its matches as rows with their lineage.
 *
 * **When the map carries a filter the panel sends the same `filters`** and shows each row's
 * `matchedCount` beside its masked one, so a filtered map and a filtered tree read the same
 * numbers. Existence and the masked count never move with the filter, and a row matching nothing
 * is still a row.
 *
 * **A click is *highlight* by default** (§5.2), because that is the action that shows where a
 * descriptor lives without losing the map; *filter* is beside it, and *fit* beside that on a layer
 * that draws something. A filter layer (§5.4) is offered here like any other and has no *fit*.
 *
 * The walk is this element's state and not the store's: what is expanded and what has been paged
 * belong to the panel, and a projection would have to hold them and be rebuilt on every tick.
 */

/** One node of the walk: a row, its children once fetched, and where its paging got to. */
type Node = {
  row: BrowseRow;
  /** The path that reached it — a `dag` node under two parents is two nodes, and this tells them apart. */
  path: string;
  children: Node[] | null;
  next: string | null;
  loading: boolean;
  refusal: Refusal | null;
};

const masked = (n: bigint): Masked => ({value: Number(n), exact: true});

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
        background: var(--tessera-surface-2);
      }
      [part='row'][data-clause='highlight'] {
        background: var(--tessera-highlight-soft);
        color: var(--tessera-highlight);
      }
      [part='row'][data-clause='filter'] {
        background: var(--tessera-accent-soft);
        color: var(--tessera-accent);
      }
      [part='expander'] {
        display: inline-flex;
        width: 14px;
        color: var(--tessera-ink-3);
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
        color: var(--tessera-ink-2);
      }
      [part='counts'] {
        display: inline-flex;
        gap: 6px;
        align-items: baseline;
        font-size: 12px;
        color: var(--tessera-ink-2);
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
        color: var(--tessera-ink-3);
        border-radius: 2px;
      }
      [part='actions'] button:hover {
        color: var(--tessera-ink);
        background: var(--tessera-surface-3);
      }
      [part='also'] {
        padding-left: calc(18px + var(--depth, 0) * 14px);
        font-size: 11px;
        color: var(--tessera-ink-3);
      }
      [part='more'] {
        padding-left: calc(18px + var(--depth, 0) * 14px);
        font-size: 12px;
        color: var(--tessera-accent);
      }
      [part='lineage'] {
        font-size: 11px;
        color: var(--tessera-ink-3);
      }
    `
  ];

  /** Which layer's hierarchy is shown; unset, the first the bundle offers. */
  @property() accessor layer = '';
  /** Rows per page, and what *N more* fetches. Clamped to the deployment's own ceiling. */
  @property({type: Number}) accessor limit = 50;

  @state() private accessor roots: Node[] | null = null;
  @state() private accessor rootsNext: string | null = null;
  @state() private accessor query = '';
  @state() private accessor searching: Node[] | null = null;
  @state() private accessor loading = false;
  @state() private accessor refusal: Refusal | null = null;
  /** Which paths are open, so a re-render of the tree keeps the walk. */
  @state() private accessor open = new Set<string>();
  /** Bumped to re-render after a node's children land — the tree is held in `roots`, by mutation. */
  @state() private accessor revision = 0;

  /** What the roots were fetched under, so a filter or a layer change refetches and nothing else does. */
  private fetchedUnder = '';
  /**
   * Every name the walk has seen, by identifier. **The only place a name for one of these
   * artifacts exists on this client**: a filter layer is never named in a viewport request, so its
   * artifacts are never served, and *also under 546790* says nothing about what a node sits under.
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
   * The question the panel is asking, as a string: the layer and the map's own filter. A change
   * refetches the roots and drops the walk, because every count in it answers the old question.
   */
  private question(): string {
    const s = this.resolvedStore;
    const filters = s?.get('filters');
    // The member clauses are written out by hand: they carry `bigint` identifiers, which
    // `JSON.stringify` refuses outright rather than approximating.
    const members = (filters?.members ?? []).map((c) => `${c.layer}:${c.artifact}:${c.outside ? 'out' : 'in'}:${c.verb}`).join(',');
    return `${this.current()?.name ?? ''}|${JSON.stringify(filters?.expr ?? null)}|${members}`;
  }

  protected override onStoreChange(): void {
    const q = this.question();
    if (q !== this.fetchedUnder && this.current()) {
      this.fetchedUnder = q;
      void this.loadRoots();
    }
    super.onStoreChange();
  }

  protected override updated(): void {
    if (this.roots === null && this.current() && this.resolvedStore && !this.loading && this.fetchedUnder === '') {
      this.fetchedUnder = this.question();
      void this.loadRoots();
    }
  }

  /** One page of the current layer, under the map's own filter — the store supplies that. */
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

  /** What to call an artifact this walk has met; its identifier where the walk has not. */
  private nameOf(id: bigint): string {
    return this.names.get(id) ?? idString(id);
  }

  /**
   * Expand a node: its children, paged. A node already opened keeps what it fetched — closing and
   * reopening costs nothing, and the counts are still answers to the same question, which
   * {@link question} is what guards.
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
    // Debounced like every other typed control here: a scan over the layer's keys and names is
    // bounded by its artifact count and never by the corpus, but it is still a request a keystroke.
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

  /** The clause this row carries, if any — what colours the row and what the buttons toggle. */
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

  override render() {
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
    const name = node.row.name ?? node.row.key ?? null;
    // A `dag` node under several served parents is drawn under each of them; the row says which
    // others it sits under, so the duplication reads as the structure it is (§5.1).
    const also = node.row.parentIds.filter((p) => String(p) !== node.path.split('/').at(-2));
    const drawn = !isFilterLayer(layer);
    return html`<li>
      <div part="row" style=${`--depth:${depth}`} data-id=${idString(node.row.tesseraId)} data-clause=${clause ?? nothing}>
        <button part="expander" type="button" data-leaf=${layer.hierarchy.kind === 'flat' ? '' : nothing}
          aria-expanded=${open ? 'true' : 'false'}
          aria-label=${open ? `Collapse ${name ?? 'row'}` : `Expand ${name ?? 'row'}`}
          @click=${() => this.toggle(node)}>${icon(open ? 'chev' : 'chevr', 12)}</button>
        <button part="name" type="button" data-unnamed=${name === null ? '' : nothing}
          title="Highlight this — the map stays and its members are lit"
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
            // No *fit* on a filter layer: its artifacts are spread across the frame and there is
            // nothing to fit to (§5.4).
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

function refusalOf(error: unknown): Refusal {
  const e = error as {code?: string; detail?: string; message?: string};
  return {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)};
}

attachContextRoot();
defineOnce('tessera-hierarchy', TesseraHierarchy);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-hierarchy': TesseraHierarchy;
  }
}
