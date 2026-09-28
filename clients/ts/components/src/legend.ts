import {css, html, nothing, type TemplateResult} from 'lit';
import {property, state} from 'lit/decorators.js';
import {CLUSTER_PREFIX, colourLayers, type Rgba} from '@tesseradb/client';
import {NEUTRAL} from '@tesseradb/client/internal';
import {UNMAPPED, artifactName, clusterLayerOf, colourOfFraction, colourOfRank, css as rgb, paletteValues} from '@tesseradb/deck/internal';
import {TesseraElement, UNNAMED, columnCaption, dateText, emit} from './base.js';
import {icon} from './icons.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/** The entries shown where `limit` is 0, as many as a card can hold. */
const ENTRIES_SHOWN = 40;

/**
 * What the colours mean, under a "Colour" heading. For a category column, the values the marks on
 * screen carry, by title, in their colours; for a numeric column, a ramp over the range of the
 * marks served, with its minimum and maximum; under colour by cluster, the served artifacts in
 * their colours. Entries sit in two columns. The first `limit` entries show, 40 where `limit` is
 * 0, with an "N more" button that shows the rest until the colouring changes.
 *
 * `selectable` puts the *Colour by* choice in the heading, drawn as text with a chevron. It offers
 * the rendered columns and every layer that can colour, drawn or not; colouring by a layer does not
 * draw it. A *Level* choice appears under colour by a levelled layer with several levels served.
 * Which layers are drawn is `<tessera-layer-picker>`'s.
 *
 * @summary What the map's colours mean, and the colour controls.
 * @tagname tessera-legend
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-colourchange']>} tessera-colourchange - The Colour
 *   by choice changed.
 * @fires {CustomEvent<TesseraEventDetails['tessera-levelchange']>} tessera-levelchange - The Level
 *   choice, or a levelled layer's entry in Colour by, chose a level.
 * @csspart title - The heading, holding the Colour by choice under `selectable`.
 * @csspart state - The state line, with `data-state`.
 * @csspart refusal - "Values unavailable" where the names were refused, or "Column unavailable"
 *   where the column is not rendered, with `data-code` where the server gave one.
 * @csspart select - The Colour by select.
 * @csspart cluster-option - An entry in Colour by for a layer, or for one level of a levelled layer.
 * @csspart level-select - The Level select.
 * @csspart swatches - The list of colours.
 * @csspart swatch - One colour.
 * @csspart more - The "N more" button, where there are more entries than show.
 * @csspart ramp - A numeric column's ramp.
 * @csspart label - The ramp's `min` and `max` captions.
 * @csspart value - The ramp's minimum and maximum.
 */
export class TesseraLegend extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='title'] {
        margin-bottom: 10px;
      }
      [part='title'] .choice {
        font-size: 12px;
        font-weight: 500;
        letter-spacing: 0;
        text-transform: none;
        color: var(--_tessera-ink);
      }
      .level {
        display: flex;
        align-items: center;
        gap: 8px;
        margin: -2px 0 10px;
        font-size: 12px;
      }
      .level .choice {
        font-weight: 500;
      }
      [part='swatches'] {
        display: grid;
        grid-template-columns: repeat(2, minmax(0, 1fr));
        gap: 4px 12px;
        max-height: 220px;
        overflow-y: auto;
        font-size: 12px;
      }
      .entry {
        display: grid;
        grid-template-columns: 10px minmax(0, 1fr);
        align-items: center;
        column-gap: 7px;
        min-height: 18px;
      }
      [part='swatch'] {
        display: inline-block;
        width: 10px;
        height: 10px;
        border-radius: 2px;
      }
      .entry .v {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      [part='more'] {
        margin-top: 8px;
      }
      [part='ramp'] {
        height: 8px;
        border-radius: 4px;
        margin: 2px 0 6px;
      }
    `
  ];

  /** Puts the Colour by choice in the heading. */
  @property({type: Boolean}) accessor selectable = false;
  /** Under `selectable`, also renders the swatches or the ramp below the heading. */
  @property({type: Boolean}) accessor readout = false;
  /** How many entries show before "N more" offers the rest; 0 shows the first 40. */
  @property({type: Number}) accessor limit = 0;
  /** Whether "N more" was pressed, for the colouring it was pressed under. @internal */
  @state() accessor expanded = false;
  private expandedFor: string | null = null;

  protected override onStoreChange(): void {
    // A new colouring is a new list, which starts cut again.
    const colourBy = this.resolvedStore?.get('legend').colourBy ?? null;
    if (colourBy !== this.expandedFor) {
      this.expandedFor = colourBy;
      this.expanded = false;
    }
    super.onStoreChange();
  }

  private choose(value: string): void {
    const s = this.resolvedStore;
    if (!s) return;
    // `cluster:<layer>@<level>` chooses a levelled layer's colouring and its level together.
    const at = value.lastIndexOf('@');
    const chosen = value === '' ? null : at > 0 ? value.slice(0, at) : value;
    s.setColourBy(chosen);
    emit(this, 'tessera-colourchange', {colourBy: chosen});
    if (at > 0) this.chooseLevel(value.slice(at + 1));
  }

  /** The level chosen in the Level select; `null` chooses the level drawn by default. */
  @property({type: Number, attribute: 'cluster-level'}) accessor level: number | null = null;
  /** The level drawn when none is chosen, named in the Level select's first entry; `null` names the deepest served. */
  @property({type: Number, attribute: false}) accessor autoLevel: number | null = null;

  private chooseLevel(value: string): void {
    const level = value === '' ? null : Number(value);
    this.level = level;
    emit(this, 'tessera-levelchange', {level});
  }

  override render(): TemplateResult | typeof nothing {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel"><h2 part="title">Colour</h2>${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const legend = s.get('legend');
    const artifacts = s.get('artifacts');
    const columns = meta.declaredScalars.filter((c) => c.render);
    const colourBy = legend.colourBy;
    // The level options are the rungs present among the colouring layer's served artifacts,
    // titled from `meta.levels` where the layer is levelled, else "Level N". The server computes
    // `rung` per layer kind, so a tree's depths are offered too.
    const cluster = clusterLayerOf(colourBy);
    const clusterMeta = cluster ? meta.layers.find((l) => l.name === cluster) : null;
    const rungs = new Set<number>();
    if (cluster) {
      for (const x of artifacts.colourServed) rungs.add(x.rung);
    }
    const levelsServed = [...rungs].sort((x, y) => x - y);
    const levelTitle = (l: number) => clusterMeta?.levels.find((x) => x.level === l)?.title ?? `Level ${l}`;
    // The level drawn: the one chosen, else the explorer's, else the deepest served.
    const drawnLevel = this.level ?? this.autoLevel ?? levelsServed.at(-1) ?? null;
    const choice = this.selectable
      ? html`<span class="choice"><select part="select" aria-label="Colour by" @change=${(e: Event) => this.choose((e.target as HTMLSelectElement).value)}>
            <option value="" ?selected=${colourBy === null}>None</option>
            ${colourLayers(meta.layers).flatMap((decl) => {
              // A levelled layer is offered once per level, by the declared level titles, since
              // colouring it is membership at one level. Any other layer is offered once.
              const value = `${CLUSTER_PREFIX}${decl.name}`;
              if (decl.levels.length === 0) {
                return [html`<option part="cluster-option" value=${value} ?selected=${colourBy === value}>${decl.title || decl.name}</option>`];
              }
              return decl.levels.map(
                (lv) =>
                  html`<option part="cluster-option" value=${`${value}@${lv.level}`} ?selected=${colourBy === value && (drawnLevel ?? decl.levels.at(-1)!.level) === lv.level}>${lv.title || `Level ${lv.level}`}</option>`
              );
            })}
            ${columns.map((c) => html`<option value=${c.name} ?selected=${colourBy === c.name}>${columnCaption(c.name)}</option>`)}
          </select>${icon('chev', 12, 1.4)}</span>`
      : nothing;
    const levelSelect =
      this.selectable && cluster && levelsServed.length > 1
        ? html`<div class="level"><span class="muted">Level</span><span class="choice">
            <select part="level-select" aria-label="Level" @change=${(e: Event) => this.chooseLevel((e.target as HTMLSelectElement).value)}>
              <option value="" ?selected=${this.level === null}>${this.autoLevel === null ? 'Deepest' : `Automatic (${levelTitle(this.autoLevel)})`}</option>
              ${levelsServed.map((l) => html`<option value=${l} ?selected=${this.level === l}>${levelTitle(l)}</option>`)}
            </select>${icon('chev', 12, 1.4)}</span></div>`
        : nothing;
    const heading = html`<h2 part="title">Colour${choice}</h2>`;
    const wrap = (body: unknown) => html`<div class="panel">${heading}${levelSelect}${body}</div>`;
    if (!this.readout && this.selectable) return wrap(html`<span part="state" data-state="shown"></span>`);
    if (colourBy === null) return wrap(html`<span part="state" data-state="shown"></span>`);

    const entry = (c: Rgba | readonly number[], text: string, title = text) =>
      html`<div class="entry"><span part="swatch" style=${`background:${rgb(c as Rgba)}`}></span><span class="v" title=${title}>${text}</span></div>`;
    const clusterLayer = clusterLayerOf(colourBy);
    if (clusterLayer) {
      const named = artifacts.colourServed;
      return wrap(html`<span part="state" data-state="shown"></span>${this.swatches([
        ...named.map((a) => entry(artifacts.colours.get(artifacts.table.ordinalOf(a.layer, a.tesseraId)) ?? NEUTRAL, artifactName(a) ?? UNNAMED)),
        entry(NEUTRAL, 'Not yet coloured')
      ])}`);
    }
    const column = columns.find((c) => c.name === colourBy);
    if (!column) return wrap(html`<span part="state" data-state="refused"><span class="dot refuse"></span><span part="refusal">Column unavailable</span></span>`);
    const error = legend.categoryErrors[colourBy];
    if (error) return wrap(html`<span part="state" data-state="refused"><span class="dot refuse"></span><span part="refusal" data-code=${error.code}>Values unavailable</span></span>`);
    if (column.category) {
      const values = legend.categories[colourBy];
      if (!values) return wrap(renderState('loading', s.get('status')));
      const shown = paletteValues(values, legend.ranks[colourBy] ?? {});
      // Values past the palette share one colour, named once.
      return wrap(html`<span part="state" data-state="shown"></span>${this.swatches([
        ...shown.map(({value, rank}) => entry(colourOfRank(rank), value.title ?? value.key, value.key)),
        entry(UNMAPPED, 'Other')
      ])}`);
    }
    const domain = legend.domains[colourBy];
    if (!domain) return wrap(html`<span part="state" data-state="empty">Nothing in view</span>`);
    const stops = Array.from({length: 12}, (_, i) => rgb(colourOfFraction(i / 11))).join(', ');
    const fmt = (n: number) => {
      if (column.arrowType === 'timestamp_us') return dateText(n);
      if (Math.abs(n) >= 1e6 || (n !== 0 && Math.abs(n) < 1e-3)) return n.toExponential(2);
      return n.toLocaleString('en-GB');
    };
    return wrap(html`<span part="state" data-state="shown"></span>
      <div part="ramp" style=${`background:linear-gradient(to right, ${stops})`}></div>
      <div class="kv sm"><span part="label">Lowest</span><span part="value" class="v">${fmt(domain.min)}</span><span part="label">Highest</span><span part="value" class="v">${fmt(domain.max)}</span></div>`);
  }

  /** The entries in two columns, the first of them until "N more" is pressed. */
  private swatches(entries: TemplateResult[]): TemplateResult {
    const first = this.limit > 0 ? this.limit : ENTRIES_SHOWN;
    const cut = !this.expanded && entries.length > first;
    const shown = cut ? entries.slice(0, first) : entries;
    return html`<div part="swatches">${shown}</div>${cut
      ? html`<button part="more" class="more-link" type="button" @click=${() => (this.expanded = true)}>${(entries.length - first).toLocaleString('en-GB')} more</button>`
      : nothing}`;
  }
}

attachContextRoot();
defineOnce('tessera-legend', TesseraLegend);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-legend': TesseraLegend;
  }
}
