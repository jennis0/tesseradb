import {css, html, nothing, type PropertyValues, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {repeat} from 'lit/directives/repeat.js';
import {artifactName, withMember, withoutMember, type BrowseRow, type ClauseVerb, type Layer, type MemberClause, type Refusal} from '@tesseradb/client';
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
/** How many parents a match's path is walked up, nearest first. */
const PATH_DEPTH = 3;

/** One cluster the list offers: its row and, once walked, its path of parents' names, nearest last. */
type Offer = {row: BrowseRow; path: string[] | null; beyond: boolean};

/**
 * The clusters of one layer as a filter field: a search box whose list offers the layer's
 * artifacts, and the clusters chosen as chips under it, each a `member_of` clause
 * (`Store.setMembers`) in the position `verb` names.
 *
 * With the box empty, the list offers the layer's top-level clusters (its roots, or its first level
 * on a levelled layer), largest first. Typing searches the layer's names through
 * `POST /v1/artifacts/browse`, every level on a levelled layer, and each match shows its path, the
 * names of its nearest parents in grey, cut from the left so the nearest stay. On a levelled layer
 * the path names only parents the list has already met. Each row's count is exact, from the
 * aggregate route (`Store.setAggregate`) over the rows listed, one grouping per level on a layer
 * with several: under the store's filters without this layer's own filter clauses in the filter
 * position, so a cluster its clauses exclude is still counted, and under the whole filter in the
 * highlight position, where a cluster counted 0 cannot be chosen. A row shows no count until the
 * answer lands. The list opens over what sits below the box, in the top layer.
 *
 * Choosing a row puts its clause on, with the row's name as the clause's label, and pressing a
 * chosen row takes it off. A chip names its cluster by its label, "Outside" before it where the
 * clause selects everything outside; its × takes the clause off. The arrow keys move through the
 * list, Enter chooses the row reached, the first by default, and Escape closes the list.
 *
 * @summary One layer's clusters as a filter field.
 * @tagname tessera-cluster-filter
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-clausechange']>} tessera-clausechange - A row or
 *   a chip's × put a `member_of` clause on or took it off.
 * @csspart label - The layer's title.
 * @csspart entry - The search box.
 * @csspart values - The list, while it is open.
 * @csspart option - One cluster in the list, with `data-id`, `aria-selected` while its clause is on,
 *   and `aria-disabled` where it cannot be chosen.
 * @csspart name - A row's name, with `data-unnamed` where it has none.
 * @csspart path - A row's path of parents' names.
 * @csspart value-count - A row's exact count.
 * @csspart more - The note under the box: no match, or that more match than the list holds.
 * @csspart refusal - The words "Clusters unavailable", with `data-code`, where the list was refused.
 * @csspart chosen - A chosen cluster's chip, with `data-id`.
 */
export class TesseraClusterFilter extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      .head {
        display: flex;
        align-items: baseline;
        justify-content: space-between;
        gap: 8px;
        margin-bottom: 8px;
      }
      [part='label'] {
        font-weight: 600;
        color: var(--_tessera-ink);
      }
      :host([verb='highlight']) .input:focus-within {
        outline: 0;
        border: 1.5px solid var(--_tessera-highlight);
        padding: 0 9.5px;
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
      :host([verb='highlight']) [part~='option'][aria-selected='true'] {
        background: var(--_tessera-highlight-soft);
        color: var(--_tessera-highlight);
      }
      [part~='option'][aria-disabled='true'] {
        cursor: default;
        background: none;
        color: var(--_tessera-ink-3);
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
      /* Cut from the left, so the nearest parents stay. */
      [part='path'] {
        direction: rtl;
        text-align: left;
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
      .chosen {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
        margin-top: 8px;
      }
    `
  ];

  /** The layer whose clusters are offered, a name `meta.layers` lists. Unset or unknown, the field renders nothing. */
  @property() accessor layer = '';
  /** The position the field's clauses join: `filter` or `highlight`. */
  @property({reflect: true}) accessor verb: ClauseVerb = 'filter';

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
    return (this.resolvedStore?.get('filters').members ?? []).filter((m) => m.layer === this.layer && m.verb === this.verb);
  }

  private toggle(row: BrowseRow): void {
    const s = this.resolvedStore;
    if (!s) return;
    const members = s.get('filters').members;
    const on = this.held().some((m) => m.artifact === row.tesseraId && !m.outside);
    const name = artifactName(row);
    s.setMembers(
      on
        ? withoutMember(members, this.layer, row.tesseraId, this.verb)
        : withMember(members, {layer: this.layer, artifact: row.tesseraId, outside: false, verb: this.verb, ...(name === null ? {} : {label: name})})
    );
    emit(this, 'tessera-clausechange', {id: idString(row.tesseraId), layer: this.layer, outside: false, verb: this.verb, on: !on});
    this.search = '';
    this.found = null;
    this.listOpen = false;
  }

  private drop(clause: MemberClause): void {
    const s = this.resolvedStore;
    if (!s) return;
    s.setMembers(withoutMember(s.get('filters').members, clause.layer, clause.artifact, clause.verb));
    emit(this, 'tessera-clausechange', {id: idString(clause.artifact), layer: clause.layer, outside: clause.outside, verb: clause.verb, on: false});
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

  protected override updated(changed: PropertyValues<this>): void {
    super.updated(changed);
    this.floating.update();
    // Counted only while the list is open: a closed list shows no counts, so asks for none.
    const layer = this.declared();
    const limits = this.resolvedStore?.get('meta')?.selection;
    const offers = this.listOpen && this.isConnected ? (this.listed() ?? []) : [];
    this.counts.set(
      this.resolvedStore,
      layer && limits && offers.length > 0
        ? {groupings: artifactGroupings(layer, offers.map((o) => o.row), limits), ...(this.verb === 'filter' ? {withoutMembersOf: layer.name} : {})}
        : null
    );
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const layer = this.declared();
    if (!s || !layer) return nothing;
    const title = layer.title || layer.name;
    const chosen = this.held();
    const on = new Set(chosen.filter((m) => !m.outside).map((m) => m.artifact));
    const counts = countsByKey(this.counts.entry());
    const countOf = (o: Offer) => counts?.get(o.row.tesseraId.toString());
    const listed = this.listed();
    // Largest first once the counts land; the server's order until then.
    const offers = listed === null ? [] : counts === null ? listed : [...listed].sort((a, b) => (countOf(b) ?? -1) - (countOf(a) ?? -1));
    const out = (o: Offer) => this.verb === 'highlight' && countOf(o) === 0 && !on.has(o.row.tesseraId);
    const choosable = offers.filter((o) => !out(o));
    const active = choosable.find((o) => o.row.tesseraId === this.activeId) ?? choosable[0] ?? null;
    const move = (by: 1 | -1) => {
      if (choosable.length === 0) return;
      const at = active ? choosable.indexOf(active) : -1;
      this.activeId = choosable[(at + by + choosable.length) % choosable.length]!.row.tesseraId;
    };
    const open = this.listOpen && offers.length > 0;
    const option = (o: Offer) => {
      const name = artifactName(o.row);
      const count = countOf(o);
      const left = out(o);
      const path = o.path && o.path.length > 0 ? `${o.beyond ? '… › ' : ''}${o.path.join(' › ')}` : '';
      return html`<button type="button" part="option" role="option" id=${`c-${o.row.tesseraId}`} tabindex="-1" data-id=${idString(o.row.tesseraId)} ?data-active=${o === active}
        aria-selected=${on.has(o.row.tesseraId) ? 'true' : 'false'} aria-disabled=${left ? 'true' : 'false'}
        @mousedown=${(e: Event) => e.preventDefault()} @click=${() => !left && this.toggle(o.row)}>
        <span class="opt"><span part="name" ?data-unnamed=${name === null}>${name ?? UNNAMED}</span>${path ? html`<span part="path" title=${path}><bdi>${path}</bdi></span>` : nothing}</span>
        ${count === undefined ? nothing : html`<span part="value-count">${count.toLocaleString('en-GB')}</span>`}
      </button>`;
    };
    const box = html`<div class="combo">
      <div class="input">${icon('search', 14)}<input id="ctl" part="entry" type="search" autocomplete="off" placeholder="Type a name"
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
            if (open && active) this.toggle(active.row);
          } else if (e.key === 'Escape' && this.listOpen) {
            e.stopPropagation();
            this.listOpen = false;
          }
        }} /></div>
      ${open ? html`<div part="values" id="values" popover="manual" role="listbox" aria-label=${`${title} clusters`}>${repeat(offers, (o) => o.row.tesseraId, option)}</div>` : nothing}
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
    const chips =
      chosen.length > 0
        ? html`<div class="chosen">
            ${chosen.map((m) => {
              const text = `${m.outside ? 'Outside ' : ''}${m.label ?? UNNAMED}`;
              return html`<span part="chosen" class="chip" data-verb=${this.verb} data-id=${idString(m.artifact)}>${text}<button type="button" aria-label=${`Remove ${text}`} @click=${() => this.drop(m)}>${icon('close', 12)}</button></span>`;
            })}
          </div>`
        : nothing;
    return html`<div class="head"><label part="label" for="ctl">${title}</label></div>${box}${note}${chips}`;
  }
}

attachContextRoot();
defineOnce('tessera-cluster-filter', TesseraClusterFilter);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-cluster-filter': TesseraClusterFilter;
  }
}
