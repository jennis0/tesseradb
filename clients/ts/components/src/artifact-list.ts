import {css, html, nothing, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {artifactName, withMember, withoutMember, type Artifact, type ArtifactsProjection, type ClauseVerb, type Meta} from '@tesseradb/client';
import {NEUTRAL} from '@tesseradb/client/internal';
import {clusterLayerOf, css as rgb} from '@tesseradb/deck/internal';
import {HeldAggregate, artifactGroupings, countsByKey} from './aggregate.js';
import {TesseraElement, UNNAMED, emit, idString} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {refusalText, renderState} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * The clusters on screen at the level the map draws, as a flat list, largest first: each row its
 * colour, its name, its parent's name in grey, and its exact count under the store's filters. The
 * list is the colouring layer's served artifacts while the points are coloured by a layer, else the
 * drawn layers'. `level` is the level the map draws at: on a layer that declares levels the rows
 * are that level's artifacts, the deepest served where `level` is unset; on any other layer they
 * are the deepest artifact served in each branch at or above `level`, every branch's deepest where
 * it is unset. An artifact with no name shows a placeholder; a key is never shown.
 *
 * Pressing a row fires `tessera-artifactfit`, which `<tessera-explorer>` answers by fitting its map
 * to the artifact. Hovering or focusing a row shows its Highlight and Filter buttons, which put a
 * `member_of` clause on its artifact in that position, named by the row's name; a pressed button
 * stays shown as a ×, which takes the clause off, and fills the row.
 *
 * The counts are the server's: the list keeps an aggregate registered with the store
 * (`Store.setAggregate`) over the artifacts it lists, the same count the legend shows for the same
 * artifact. A count is over the artifact's whole membership that passes the filters, not the part
 * in view; the view decides only whether an artifact is listed. Until the answer lands the rows
 * show no count and are ordered by their masked counts.
 *
 *
 * @summary The clusters on screen at the level drawn, with their counts.
 * @tagname tessera-artifact-list
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-artifactfit']>} tessera-artifactfit - A row was
 *   pressed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-clausechange']>} tessera-clausechange - A row's
 *   Highlight or Filter button put a `member_of` clause on or took it off.
 * @csspart title - The heading, with how many are listed.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - The words "Layer unavailable", with `data-code` set to the refusal's code.
 * @csspart items - The list.
 * @csspart item - One artifact, with `data-id`, and `data-clause` while a clause is on it:
 *   `filter`, `highlight`, or both separated by a space.
 * @csspart swatch - An artifact's colour on the map.
 * @csspart name - An artifact's name, with `data-unnamed` where it has none.
 * @csspart parent - The name of an artifact's parent, where one is served.
 * @csspart count - An artifact's exact count, once the server's answer lands.
 * @csspart highlight - A row's Highlight button, with `aria-pressed`.
 * @csspart filter - A row's Filter button, with `aria-pressed`.
 * @csspart more - The "N more" button, which shows the next `rows` rows until the layers change.
 * @cssprop --tessera-artifact-list-height - The tallest the list grows before it scrolls, 320 px
 *   unless set.
 */
export class TesseraArtifactList extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='more'] {
        margin: 6px 6px 0;
      }
      [part='items'] {
        list-style: none;
        margin: 0;
        padding: 0;
        max-height: var(--tessera-artifact-list-height, 320px);
        overflow-y: auto;
      }
      [part~='item'] {
        position: relative;
        display: grid;
        grid-template-columns: 10px minmax(0, 1fr) auto;
        align-items: center;
        column-gap: 8px;
        padding: 5px 6px;
        border-radius: 6px;
        cursor: pointer;
      }
      [part~='item']:hover,
      [part~='item']:focus-visible,
      [part~='item'][data-clause~='filter'] {
        background: var(--_tessera-surface-2);
      }
      [part~='item'][data-clause~='highlight'] {
        background: var(--_tessera-highlight-soft);
        color: var(--_tessera-highlight);
      }
      [part~='item']:focus-visible {
        outline-offset: -2px;
      }
      [part='swatch'] {
        width: 10px;
        height: 10px;
        border-radius: 2px;
        background: var(--c, transparent);
      }
      .names {
        display: flex;
        flex-direction: column;
        min-width: 0;
      }
      [part='name'],
      [part='parent'] {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      [part='name'][data-unnamed] {
        color: var(--_tessera-ink-3);
      }
      [part='parent'] {
        font-size: 12px;
        color: var(--_tessera-ink-3);
      }
      [part='count'] {
        color: var(--_tessera-ink-2);
        font-size: 12px;
        font-variant-numeric: tabular-nums;
      }
      /* The buttons sit over the row's end, on the row's own fill, so showing them moves nothing. */
      .verbs {
        position: absolute;
        top: 50%;
        right: 4px;
        transform: translateY(-50%);
        display: flex;
        gap: 2px;
        padding-left: 4px;
        border-radius: 5px;
        background: inherit;
      }
      .verbs button {
        width: 22px;
        height: 22px;
        display: grid;
        place-items: center;
        border-radius: 4px;
        color: var(--_tessera-ink-2);
      }
      .verbs button[aria-pressed='false'] {
        display: none;
      }
      [part~='item']:hover .verbs button,
      [part~='item']:focus-within .verbs button {
        display: grid;
      }
      .verbs button:hover {
        background: var(--_tessera-surface-3);
      }
      [part='highlight'][aria-pressed='true'] {
        color: var(--_tessera-highlight);
      }
    `
  ];

  /** The served artifacts to list in place of the store's `artifacts` projection. */
  @property({attribute: false}) accessor artifacts: ArtifactsProjection | null = null;
  /** How many rows the list shows before it offers the rest under "N more". */
  @property({type: Number}) accessor rows = 40;
  /** The level the map draws at; unset, the deepest served. */
  @property({type: Number}) accessor level: number | null = null;
  /** Rows shown beyond `rows` after "N more" was pressed, for the layers it was pressed under. */
  @state() private accessor extra = 0;
  private extraFor = '';
  private readonly counts = new HeldAggregate('in-view');
  /** The artifacts the list shows, as last rendered, which its aggregate counts. */
  private listedNow: Artifact[] = [];

  override disconnectedCallback(): void {
    this.counts.set(null, null);
    super.disconnectedCallback();
  }

  protected override updated(): void {
    const s = this.resolvedStore;
    const meta = s?.get('meta');
    const layer = meta?.layers.find((l) => l.name === this.listedNow[0]?.layer);
    const own = this.isConnected && this.artifacts === null && layer && this.listedNow.every((a) => a.layer === layer.name);
    this.counts.set(s, own && meta && this.listedNow.length > 0 ? {groupings: artifactGroupings(layer, this.listedNow, meta.selection)} : null);
  }

  protected override onStoreChange(): void {
    // Other layers are another list, which starts at `rows` again.
    const layers = this.shown?.layers.join(' ') ?? '';
    if (layers !== this.extraFor) {
      this.extraFor = layers;
      this.extra = 0;
    }
    super.onStoreChange();
  }

  private get shown(): ArtifactsProjection | null {
    return this.artifacts ?? this.resolvedStore?.get('artifacts') ?? null;
  }

  private fit(a: Artifact): void {
    emit(this, 'tessera-artifactfit', {id: idString(a.tesseraId)});
  }

  /** The positions of the clauses on an artifact. */
  private clausesOn(a: Artifact): ClauseVerb[] {
    const held = this.resolvedStore?.get('filters').members ?? [];
    return held.filter((c) => c.layer === a.layer && c.artifact === a.tesseraId && !c.outside).map((c) => c.verb);
  }

  private apply(a: Artifact, verb: ClauseVerb, name: string | null): void {
    const s = this.resolvedStore;
    if (!s) return;
    const held = s.get('filters').members;
    const on = this.clausesOn(a).includes(verb);
    s.setMembers(
      on
        ? withoutMember(held, a.layer, a.tesseraId, verb)
        : withMember(held, {layer: a.layer, artifact: a.tesseraId, outside: false, verb, ...(name === null ? {} : {label: name})})
    );
    emit(this, 'tessera-clausechange', {id: idString(a.tesseraId), layer: a.layer, outside: false, verb, on: !on});
  }

  override render(): TemplateResult | typeof nothing {
    const a = this.shown;
    const heading = (summary: unknown = nothing) => html`<h2 part="title">In view<span class="summary">${summary}</span></h2>`;
    if (!a) return html`<div class="panel">${heading()}${renderState('detached', null)}</div>`;
    const meta = this.resolvedStore?.get('meta') ?? null;
    const colourLayer = clusterLayerOf(this.resolvedStore?.get('legend').colourBy ?? null);
    const source = colourLayer ? a.colourServed.filter((x) => x.layer === colourLayer) : a.served;
    if (a.layers.length === 0 && !colourLayer) return html`<div class="panel">${heading()}<span part="state" data-state="empty">No layer on</span></div>`;
    if (a.status === 'idle' || a.status === 'loading') return html`<div class="panel">${heading()}${renderState('loading', this.resolvedStore?.get('status') ?? null)}</div>`;
    if (a.status === 'refused') {
      return html`<div class="panel">${heading()}<span part="state" data-state="refused"><span class="dot refuse"></span>${refusalText('Layer unavailable', a.refusal?.code)}</span></div>`;
    }
    const byMasked = listedAt(source, this.level, meta, colourLayer === null);
    this.listedNow = byMasked;
    if (byMasked.length === 0) return html`<div class="panel">${heading()}<span part="state" data-state="empty">Nothing in view</span></div>`;
    const counts = countsByKey(this.counts.entry());
    const countOf = (x: Artifact) => counts?.get(x.tesseraId.toString());
    // Largest first by the exact counts once they land; the masked counts' order until then.
    const listed = counts === null ? byMasked : [...byMasked].sort((x, y) => (countOf(y) ?? -1) - (countOf(x) ?? -1));
    const byId = new Map(source.map((x) => [x.tesseraId, x]));
    const shown = listed.slice(0, this.rows + this.extra);
    const n = listed.length;
    const filterable = this.resolvedStore !== null && this.artifacts === null;
    return html`<div class="panel">${heading(`${n.toLocaleString('en-GB')} cluster${n === 1 ? '' : 's'}`)}
      <span part="state" data-state="shown"></span>
      <ul part="items" class="list">
        ${shown.map((artifact) => {
          const name = artifactName(artifact, a.attached);
          const up = artifact.parentIds.map((p) => byId.get(p)).find((p) => p !== undefined);
          const parent = up ? (artifactName(up, a.attached) ?? UNNAMED) : null;
          const count = countOf(artifact);
          const colour = a.colours.get(a.table.ordinalOf(artifact.layer, artifact.tesseraId)) ?? NEUTRAL;
          const clauses = this.clausesOn(artifact);
          const title = name ?? UNNAMED;
          const verb = (v: ClauseVerb) => {
            const on = clauses.includes(v);
            const words = v === 'filter' ? (on ? `Stop filtering to ${title}` : `Filter to ${title}`) : on ? `Stop highlighting ${title}` : `Highlight ${title}`;
            return html`<button part=${v === 'filter' ? 'filter' : 'highlight'} type="button" aria-pressed=${on ? 'true' : 'false'} aria-label=${words} title=${words}
              @click=${(e: Event) => {
                e.stopPropagation();
                this.apply(artifact, v, name);
              }} @keydown=${(e: KeyboardEvent) => e.stopPropagation()}>${on ? icon('close', 12) : v === 'filter' ? icon('filter', 13, 1.5) : icon('highlight', 13)}</button>`;
          };
          return html`<li
            part="item"
            role="button"
            tabindex="0"
            data-id=${idString(artifact.tesseraId)}
            data-clause=${clauses.length > 0 ? [...clauses].sort().join(' ') : nothing}
            title=${`Fit the map to ${title}`}
            @click=${() => this.fit(artifact)}
            @keydown=${(e: KeyboardEvent) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                this.fit(artifact);
              }
            }}
          >
            <span part="swatch" style=${`--c:${rgb(colour)}`}></span>
            <span class="names"><span part="name" ?data-unnamed=${name === null}>${title}</span>${parent === null ? nothing : html`<span part="parent">${parent}</span>`}</span>
            <span part="count">${count === undefined ? '' : count.toLocaleString('en-GB')}</span>
            ${filterable ? html`<span class="verbs">${verb('highlight')}${verb('filter')}</span>` : nothing}
          </li>`;
        })}
      </ul>
      ${listed.length > shown.length
        ? html`<button class="more-link" part="more" type="button" @click=${() => (this.extra += this.rows)}>${(listed.length - shown.length).toLocaleString('en-GB')} more</button>`
        : nothing}
    </div>`;
  }
}

/**
 * The artifacts of `source` the map draws at `level`, largest first, ties by lowest id. On a
 * layer that declares levels, those of that level (the deepest present where `level` is null); on
 * any other, each branch's deepest artifact at or above `level`. With `drawn`, a dependent layer's
 * rows are left out where a served label names its target, since its text is that target's name.
 */
export function listedAt(source: readonly Artifact[], level: number | null, meta: Meta | null, drawn: boolean): Artifact[] {
  const declared = (layer: string) => meta?.layers.find((l) => l.name === layer);
  const labelled = drawn && source.some((x) => x.target !== null && x.content.length > 0);
  const rows = source.filter((x) => !labelled || !declared(x.layer)?.depsOn.length);
  const deepest = new Map<string, number>();
  for (const x of rows) deepest.set(x.layer, Math.max(deepest.get(x.layer) ?? 0, x.rung));
  const levelled = (layer: string) => (declared(layer)?.levels.length ?? 0) > 0;
  const within = rows.filter((x) => (levelled(x.layer) ? x.rung === (level ?? deepest.get(x.layer)) : level === null || x.rung <= level));
  const ids = new Set(within.map((x) => x.tesseraId));
  const parents = new Set<bigint>();
  for (const x of within) if (!levelled(x.layer)) for (const p of x.parentIds) if (ids.has(p)) parents.add(p);
  return within
    .filter((x) => !parents.has(x.tesseraId))
    .sort((x, y) => (x.maskedCount < y.maskedCount ? 1 : x.maskedCount > y.maskedCount ? -1 : x.tesseraId < y.tesseraId ? -1 : x.tesseraId > y.tesseraId ? 1 : 0));
}

attachContextRoot();
defineOnce('tessera-artifact-list', TesseraArtifactList);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-artifact-list': TesseraArtifactList;
  }
}
