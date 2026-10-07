import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {artifactName, withMember, withoutMember, type BrowseRow, type Layer, type MemberClause, type Refusal} from '@tesseradb/client';
import {refusalOf} from '@tesseradb/client/internal';
import {HeldAggregate, artifactGroupings, countsByKey} from './aggregate.js';
import {TesseraElement, UNNAMED, emit, idString} from './base.js';
import {FloatingList} from './float.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {chrome, tokens} from './tokens.js';

/** How long typing must be quiet before a name search is sent. */
const SEARCH_DEBOUNCE_MS = 250;
/** The top-level clusters asked for, to rank by count while the box is empty. */
const TOP_LEVEL_ROWS = 50;
/**
 * A path of `n` of its nearest parents, nearest last, led by an ellipsis where parents further up
 * are left out.
 */
function pathText(path: readonly string[], beyond: boolean, n: number): string {
  return `${beyond || n < path.length ? '… › ' : ''}${path.slice(path.length - n).join(' › ')}`;
}

/** How many parents a match's path is walked up, nearest first. */
const PATH_DEPTH = 3;

/** One cluster the list offers: its row and, once walked, its path of parents' names, nearest last. */
type Offer = {row: BrowseRow; path: string[] | null; beyond: boolean};

/**
 * The search box of a layer's field card: its list offers the layer's artifacts, and choosing one
 * puts its `member_of` clause on in the filter position (`Store.setMembers`).
 *
 * With the box empty, the list offers the layer's top-level clusters (its roots, or its first level
 * on a levelled layer), largest first. Typing searches the layer's names through
 * `POST /v1/artifacts/browse`, every level on a levelled layer, and each match shows its path, the
 * names of its nearest parents in grey, cut from the left so the nearest stay. On a levelled layer
 * the path names only parents the list has already met. Each row's count is exact, from the
 * aggregate route (`Store.setAggregate`) over the rows listed, one grouping per level on a layer
 * with several, under the store's filters without this layer's own filter clauses, so a cluster
 * its clauses exclude is still counted. A row shows no count until the answer lands. The list opens
 * over what sits below the box, in the top layer.
 *
 * Choosing a row puts its clause on, with the row's name as the clause's label, and pressing a
 * chosen row takes it off. The arrow keys move through the list, Enter chooses the row reached, the
 * first by default, and Escape closes the list.
 *
 * @summary A layer's field card's search box.
 * @tagname tessera-cluster-filter
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-clausechange']>} tessera-clausechange - A row put
 *   a `member_of` clause on or took it off.
 * @csspart entry - The search box.
 * @csspart values - The list, while it is open.
 * @csspart option - One cluster in the list, with `data-id` and `aria-selected` while its clause is
 *   on.
 * @csspart name - A row's name, with `data-unnamed` where it has none.
 * @csspart path - A row's path of parents' names.
 * @csspart value-count - A row's exact count.
 * @csspart more - The note under the box: no match, or that more match than the list holds.
 * @csspart refusal - The words "Clusters unavailable", with `data-code`, where the list was refused.
 */
export class TesseraClusterFilter extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      .input {
        height: 28px;
        gap: 6px;
        padding: 0 8px;
        font-size: 12px;
      }
      .input input {
        font-size: 12px;
      }
      .combo {
        position: relative;
      }
      /* The list opens over what sits below the box, in the top layer. */
      [part='values'] {
        position: fixed;
        inset: auto;
        margin: 0;
        box-sizing: border-box;
        overflow-y: auto;
        display: flex;
        flex-direction: column;
        padding: 4px;
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius-control);
        background: var(--_tessera-surface);
        box-shadow: 0 6px 18px rgba(0, 0, 0, 0.08);
      }
      [part~='option'] {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto;
        align-items: center;
        column-gap: 10px;
        padding: 5px 8px;
        border-radius: 4px;
        text-align: left;
      }
      [part~='option']:hover,
      [part~='option'][data-active] {
        background: var(--_tessera-surface-2);
      }
      [part~='option'][aria-selected='true'] {
        background: var(--_tessera-surface-2);
        font-weight: 600;
      }
      .opt {
        display: flex;
        flex-direction: column;
        min-width: 0;
      }
      [part='name'],
      [part='path'] {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      /* Shortened from the left a whole name at a time, so the nearest parents stay; see pathText. */
      [part='path'] {
        font-size: 12px;
        font-weight: 400;
        color: var(--_tessera-ink-3);
      }
      [part='value-count'] {
        font-size: 12px;
        font-weight: 400;
        font-variant-numeric: tabular-nums;
        color: var(--_tessera-ink-2);
      }
      [part='more'],
      [part='refusal'] {
        display: block;
        margin-top: 6px;
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
    `
  ];

  /** The layer whose clusters are offered, a name `meta.layers` lists. Unset or unknown, the field renders nothing. */
  @property() accessor layer = '';
  /** The search box's placeholder, in place of the field's own. */
  @property() accessor placeholder = '';
  /**
   * Where set, the box picks a cluster rather than filtering by it: choosing a row calls this with
   * its `tessera_id` as a decimal string, its name, its path of parents' names as the list shows
   * it and its rung, changes no clause and empties the box, and no row shows as chosen.
   */
  @property({attribute: false}) accessor choose: ((cluster: {id: string; name: string | null; path: string; rung: number}) => void) | null = null;

  /** @internal */
  @state() accessor search = '';
  /** Whether the list is open. @internal */
  @state() accessor listOpen = false;
  /** The row the arrow keys reached, by `tesseraId`; `null` is the first that can be chosen. @internal */
  @state() accessor activeId: bigint | null = null;
  /** The top-level clusters, once fetched. @internal */
  @state() accessor tops: Offer[] | null = null;
  /** The matches for the search asked last, once they land. @internal */
  @state() accessor found: {q: string; offers: Offer[]; more: boolean} | null = null;
  /** @internal */
  @state() accessor refusal: Refusal | null = null;

  private readonly counts = new HeldAggregate('clusters');
  private readonly floating = new FloatingList(() => {
    const list = this.renderRoot.querySelector<HTMLElement>('[part="values"]');
    const anchor = this.renderRoot.querySelector<HTMLElement>('.combo');
    return list && anchor ? {list, anchor} : null;
  });
  /** Every artifact's row as the field has met it, for names and parents. */
  private readonly rows = new Map<bigint, BrowseRow>();
  /** Each artifact's served parents as a children-form page gave them. */
  private readonly parentsOf = new Map<bigint, Promise<BrowseRow[]>>();
  /** The layer and view the top-level clusters were fetched for. */
  private fetchedFor = '';
  private searchTimer: ReturnType<typeof setTimeout> | null = null;
  /** Moved by {@link resetServerData}, so a page asked for before is dropped. */
  private epoch = 0;

  protected override resetServerData(): void {
    this.epoch += 1;
    this.tops = null;
    this.found = null;
    this.refusal = null;
    this.rows.clear();
    this.parentsOf.clear();
    this.fetchedFor = '';
  }

  override disconnectedCallback(): void {
    if (this.searchTimer) clearTimeout(this.searchTimer);
    this.searchTimer = null;
    this.counts.set(null, null);
    this.floating.stop();
    super.disconnectedCallback();
  }

  private declared(): Layer | null {
    return this.resolvedStore?.get('meta')?.layers.find((l) => l.name === this.layer) ?? null;
  }

  /** The levels a request addresses on the layer: each declared level on a levelled one, else one request with none. */
  private levels(layer: Layer): (number | undefined)[] {
    const levelled = layer.hierarchy.kind === 'stacked' || layer.hierarchy.kind === 'tiered';
    return levelled && layer.levels.length > 0 ? layer.levels.map((l) => l.level) : [undefined];
  }

  /** One browse page of the layer. Counts come from the aggregate, so the page is asked unfiltered. */
  private browse(req: {q?: string; parent?: bigint; level?: number; limit: number}) {
    const s = this.resolvedStore;
    if (!s) return Promise.reject(new Error('no store'));
    const ceiling = s.get('meta')?.selection.maxBrowseRows ?? req.limit;
    const {level, ...rest} = req;
    return s.browse({...rest, layer: this.layer, filters: null, limit: Math.max(1, Math.min(req.limit, ceiling)), ...(level === undefined ? {} : {level})});
  }

  private offer(row: BrowseRow): Offer {
    this.rows.set(row.tesseraId, row);
    return {row, path: row.parentIds.length === 0 ? [] : null, beyond: false};
  }

  /** Fetch the top-level clusters, once per layer and view. */
  private fetchTops(): void {
    const s = this.resolvedStore;
    const layer = this.declared();
    if (!s || !layer) return;
    const key = `${layer.name}|${s.get('view').id}`;
    if (key === this.fetchedFor) return;
    this.fetchedFor = key;
    const epoch = this.epoch;
    const level = this.levels(layer)[0];
    this.browse({limit: TOP_LEVEL_ROWS, ...(level === undefined ? {} : {level})})
      .then((page) => {
        if (epoch !== this.epoch) return;
        this.tops = page.artifacts.map((r) => this.offer(r));
        this.refusal = null;
      })
      .catch((error: unknown) => {
        if (epoch !== this.epoch) return;
        this.refusal = refusalOf(error);
        this.tops = [];
        // Asked again the next time the list opens.
        this.fetchedFor = '';
      });
  }

  /** Search the layer's names for `q`, at every level of a levelled layer, and walk each match's path. */
  private searchFor(q: string): void {
    const layer = this.declared();
    if (!layer) return;
    const epoch = this.epoch;
    const limit = this.resolvedStore?.get('meta')?.selection.maxBrowseRows ?? 50;
    Promise.all(this.levels(layer).map((level) => this.browse({q, limit: Math.min(limit, 50), ...(level === undefined ? {} : {level})})))
      .then((pages) => {
        if (epoch !== this.epoch || this.search.trim() !== q) return;
        const offers = pages.flatMap((p) => p.artifacts.map((r) => this.offer(r)));
        this.found = {q, offers, more: pages.some((p) => p.next !== null)};
        this.refusal = null;
        const walkable = layer.hierarchy.kind === 'nested' || layer.hierarchy.kind === 'dag';
        for (const o of offers) void this.walk(o, walkable, epoch);
      })
      .catch((error: unknown) => {
        if (epoch !== this.epoch) return;
        this.refusal = refusalOf(error);
        this.found = {q, offers: [], more: false};
      });
  }

  /**
   * Fill in an offer's path, its first parent's name and so on up to {@link PATH_DEPTH} parents. On
   * a layer whose children form needs no level, a missing parent is asked for; on a levelled layer
   * only parents already met are named.
   */
  private async walk(o: Offer, ask: boolean, epoch: number): Promise<void> {
    const names: string[] = [];
    let at: BrowseRow = o.row;
    for (let i = 0; i < PATH_DEPTH && at.parentIds.length > 0; i++) {
      const up = at.parentIds[0]!;
      let parent = this.rows.get(up);
      if (!parent && ask) {
        let held = this.parentsOf.get(at.tesseraId);
        if (!held) {
          held = this.browse({parent: at.tesseraId, limit: 1}).then((p) => p.parents, () => []);
          this.parentsOf.set(at.tesseraId, held);
        }
        for (const r of await held) this.rows.set(r.tesseraId, r);
        parent = this.rows.get(up);
      }
      if (!parent) break;
      names.unshift(artifactName(parent) ?? UNNAMED);
      at = parent;
    }
    if (epoch !== this.epoch) return;
    o.path = names;
    o.beyond = at.parentIds.length > 0;
    this.requestUpdate();
  }

  /** The clauses this field holds: the layer's, in its position. */
  private held(): MemberClause[] {
    return (this.resolvedStore?.get('filters').members ?? []).filter((m) => m.layer === this.layer && m.verb === 'filter');
  }

  private toggle(offer: Offer): void {
    const s = this.resolvedStore;
    if (!s) return;
    const row = offer.row;
    if (this.choose) {
      this.search = '';
      this.found = null;
      this.listOpen = false;
      this.choose({id: idString(row.tesseraId), name: artifactName(row), path: offer.path && offer.path.length > 0 ? pathText(offer.path, offer.beyond, offer.path.length) : '', rung: row.rung});
      return;
    }
    const members = s.get('filters').members;
    const on = this.held().some((m) => m.artifact === row.tesseraId && !m.outside);
    const name = artifactName(row);
    s.setMembers(
      on
        ? withoutMember(members, this.layer, row.tesseraId, 'filter')
        : withMember(members, {layer: this.layer, artifact: row.tesseraId, outside: false, verb: 'filter', ...(name === null ? {} : {label: name})})
    );
    emit(this, 'tessera-clausechange', {id: idString(row.tesseraId), layer: this.layer, outside: false, verb: 'filter', on: !on});
    this.search = '';
    this.found = null;
    this.listOpen = false;
  }

  private type(text: string): void {
    this.search = text;
    this.activeId = null;
    this.listOpen = true;
    if (this.searchTimer) clearTimeout(this.searchTimer);
    const q = text.trim();
    if (q === '') {
      this.found = null;
      return;
    }
    this.searchTimer = setTimeout(() => {
      this.searchTimer = null;
      this.searchFor(q);
    }, SEARCH_DEBOUNCE_MS);
  }

  /** The offers the list shows now: the matches for the box, or the top-level clusters while it is empty. */
  private listed(): Offer[] | null {
    const q = this.search.trim();
    if (q === '') return this.tops;
    return this.found && this.found.q === q ? this.found.offers : null;
  }

  /** Moves focus to the search box. */
  override focus(options?: FocusOptions): void {
    const target = this.renderRoot.querySelector<HTMLElement>('#ctl');
    if (target) target.focus(options);
    else super.focus(options);
  }

  /** How many of each offer's parents its path names, where all of them do not fit; by `tessera_id`. */
  private pathFits = new Map<bigint, number>();
  /** The rows and width {@link fitPaths} last measured. */
  private pathsMeasured = '';

  /**
   * Name as many of each listed path's nearest parents as fit its line, dropping whole names from
   * the left. Measured with the line's own font, after the list is placed.
   */
  private fitPaths(): void {
    // Only while the list shows, and only when its rows or its width changed since last measured.
    const shown = this.listOpen ? Array.from(this.renderRoot.querySelectorAll<HTMLElement>('[part="path"][data-id]')) : [];
    const key = `${shown[0]?.clientWidth ?? 0}|${shown.map((el) => `${el.dataset.id}:${el.title}`).join(',')}`;
    if (shown.length === 0 || key === this.pathsMeasured) return;
    this.pathsMeasured = key;
    const context = typeof document === 'undefined' ? null : document.createElement('canvas').getContext('2d');
    if (!context) return;
    let changed = false;
    for (const el of shown) {
      const id = BigInt(el.dataset.id!);
      const o = this.listed()?.find((x) => x.row.tesseraId === id);
      if (!o?.path || el.clientWidth === 0) continue;
      context.font = getComputedStyle(el).font;
      let n = o.path.length;
      while (n > 1 && context.measureText(pathText(o.path, o.beyond, n)).width > el.clientWidth) n -= 1;
      if ((this.pathFits.get(id) ?? o.path.length) !== n) {
        this.pathFits.set(id, n);
        changed = true;
      }
    }
    if (changed) this.requestUpdate();
  }

  protected override updated(changed: PropertyValues<this>): void {
    super.updated(changed);
    this.floating.update();
    this.fitPaths();
    // Counted only while the list is open: a closed list shows no counts, so asks for none.
    const layer = this.declared();
    const limits = this.resolvedStore?.get('meta')?.selection;
    const offers = this.listOpen && this.isConnected ? (this.listed() ?? []) : [];
    this.counts.set(
      this.resolvedStore,
      layer && limits && offers.length > 0
        ? {groupings: artifactGroupings(layer, offers.map((o) => o.row), limits), withoutMembersOf: layer.name}
        : null
    );
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const layer = this.declared();
    if (!s || !layer) return nothing;
    const title = layer.title || layer.name;
    const on = new Set(this.choose ? [] : this.held().filter((m) => !m.outside).map((m) => m.artifact));
    const counts = countsByKey(this.counts.entry());
    const countOf = (o: Offer) => counts?.get(o.row.tesseraId.toString());
    const listed = this.listed();
    // Largest first once the counts land; the server's order until then.
    const offers = listed === null ? [] : counts === null ? listed : [...listed].sort((a, b) => (countOf(b) ?? -1) - (countOf(a) ?? -1));
    const active = offers.find((o) => o.row.tesseraId === this.activeId) ?? offers[0] ?? null;
    const move = (by: 1 | -1) => {
      if (offers.length === 0) return;
      const at = active ? offers.indexOf(active) : -1;
      this.activeId = offers[(at + by + offers.length) % offers.length]!.row.tesseraId;
    };
    const open = this.listOpen && offers.length > 0;
    const option = (o: Offer) => {
      const name = artifactName(o.row);
      const count = countOf(o);
      const full = o.path && o.path.length > 0 ? pathText(o.path, o.beyond, o.path.length) : '';
      const path = o.path && o.path.length > 0 ? pathText(o.path, o.beyond, this.pathFits.get(o.row.tesseraId) ?? o.path.length) : '';
      return html`<button type="button" part="option" role="option" id=${`c-${o.row.tesseraId}`} tabindex="-1" data-id=${idString(o.row.tesseraId)} ?data-active=${o === active}
        aria-selected=${on.has(o.row.tesseraId) ? 'true' : 'false'}
        @mousedown=${(e: Event) => e.preventDefault()} @click=${() => this.toggle(o)}>
        <span class="opt"><span part="name" ?data-unnamed=${name === null}>${name ?? UNNAMED}</span>${path ? html`<span part="path" data-id=${idString(o.row.tesseraId)} title=${full}><bdi>${path}</bdi></span>` : nothing}</span>
        ${count === undefined ? nothing : html`<span part="value-count">${count.toLocaleString('en-GB')}</span>`}
      </button>`;
    };
    const box = html`<div class="combo">
      <div class="input">${icon('search', 12)}<input id="ctl" part="entry" type="search" autocomplete="off" placeholder=${this.placeholder || 'Type a name'}
        role="combobox" aria-expanded=${open ? 'true' : 'false'} aria-controls="values" aria-activedescendant=${open && active ? `c-${active.row.tesseraId}` : nothing}
        aria-label=${`${title}: find a cluster`} .value=${this.search}
        @focus=${() => {
          this.fetchTops();
          this.listOpen = true;
        }}
        @blur=${() => (this.listOpen = false)}
        @input=${(e: Event) => this.type((e.target as HTMLInputElement).value)}
        @keydown=${(e: KeyboardEvent) => {
          if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
            e.preventDefault();
            this.listOpen = true;
            move(e.key === 'ArrowDown' ? 1 : -1);
          } else if (e.key === 'Enter') {
            if (open && active) this.toggle(active);
          } else if (e.key === 'Escape' && this.listOpen) {
            e.stopPropagation();
            this.listOpen = false;
          }
        }} /></div>
      ${open ? html`<div part="values" id="values" popover="manual" role="listbox" aria-label=${title}>${repeat(offers, (o) => o.row.tesseraId, option)}</div>` : nothing}
    </div>`;
    const typed = this.search.trim() !== '';
    const note = this.refusal
      ? html`<span part="refusal" data-code=${this.refusal.code}>Clusters unavailable</span>`
      : typed && this.found?.q === this.search.trim()
        ? this.found.offers.length === 0
          ? html`<span part="more">No cluster by that name</span>`
          : this.found.more
            ? html`<span part="more">Type more to narrow the list</span>`
            : nothing
        : nothing;
    return html`${box}${note}`;
  }
}

attachContextRoot();
defineOnce('tessera-cluster-filter', TesseraClusterFilter);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-cluster-filter': TesseraClusterFilter;
  }
}
