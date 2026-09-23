import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {CLUSTER_PREFIX, NEUTRAL, colourLayers, layerEntries, type Rgba} from '@tesseradb/client';
import {UNMAPPED, artifactName, clusterLayerOf, colourOfFraction, colourOfRank, css as rgb, paletteValues} from '@tesseradb/deck';
import {TesseraElement, UNNAMED, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-legend>` — what the colours mean (design §5.3 tier 2): the values marks on screen
 * carry, resolved per column and never per vocabulary; a numeric domain as a ramp, derived from
 * the marks served and never a corpus-wide range; under cluster colour, the served artifacts in
 * their colours. `selectable` renders the boards' *Colour by* select — the rendered columns and
 * the clusters of every layer that can colour, drawn or not (exact only, decision 0099) — beside a
 * *Layers · N of M on* select that draws one layer or none. Colouring by a layer does not draw it.
 * The readout below is what the swatches say.
 */
export class TesseraLegend extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      .selects {
        display: flex;
        gap: 10px;
      }
      .selects .col {
        gap: 4px;
        flex: 1 1 0;
        min-width: 0;
      }
      [part='swatches'] {
        max-height: 180px;
        overflow-y: auto;
        margin-top: 10px;
      }
      [part='swatches'] .row {
        height: 24px;
      }
      [part='swatch'] {
        display: inline-block;
        width: 10px;
        height: 10px;
        border-radius: 2px;
        flex: none;
      }
      [part='ramp'] {
        height: 8px;
        border-radius: var(--tessera-radius);
        margin: 10px 0 4px;
      }
      .v {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
    `
  ];

  @property({type: Boolean}) accessor selectable = false;
  /** Whether the readout (swatches, ramp) renders under the selects. */
  @property({type: Boolean}) accessor readout = false;

  private choose(value: string): void {
    const s = this.resolvedStore;
    if (!s) return;
    // `cluster:<layer>@<level>` is a level of a levelled layer: the colouring is the layer's and
    // the level is chosen with it, in one gesture.
    const at = value.lastIndexOf('@');
    const chosen = value === '' ? null : at > 0 ? value.slice(0, at) : value;
    s.setColourBy(chosen);
    emit(this, 'tessera-colourchange', {colourBy: chosen});
    if (at > 0) this.chooseLevel(value.slice(at + 1));
  }

  /** The level the map colours and labels at: `null` is the deepest served (the cut's leaves). */
  @property({type: Number, attribute: 'cluster-level'}) accessor level: number | null = null;
  /** The level drawn when none is chosen (the explorer's, from the budget); shown in the first option. */
  @property({type: Number, attribute: false}) accessor autoLevel: number | null = null;

  private chooseLevel(value: string): void {
    const level = value === '' ? null : Number(value);
    this.level = level;
    emit(this, 'tessera-levelchange', {level});
  }

  private chooseLayers(value: string): void {
    const s = this.resolvedStore;
    if (!s) return;
    const roots = value === '' ? [] : [value];
    s.setLayers(roots);
    emit(this, 'tessera-layerchange', {layers: roots});
  }

  override render() {
    const s = this.resolvedStore;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`<div class="panel">${this.selectable ? nothing : html`<h2 part="title">Colour</h2>`}${renderState(stateOf(s?.get('status')), s?.get('status'))}</div>`;
    const legend = s.get('legend');
    const artifacts = s.get('artifacts');
    const columns = meta.declaredScalars.filter((c) => c.render);
    const colourBy = legend.colourBy;
    const entries = layerEntries(meta.layers);
    const on = entries.filter((e) => artifacts.layers.includes(e.root.name));
    // The level select follows what the cut served: the rungs present in the served set of the
    // colouring layer, titled from `meta.levels` where the layer is tiered, else *level N*.
    //
    // **The wire's `rung`, read off the served artifact** (contracts §3.2 r44): the declared level
    // on a levelled layer, the response-local chain depth on a treed one — so a treed layer, whose
    // every artifact is declared at level 0, still offers the rungs it actually has (the demo's own
    // `clusters/hdbscan`), and nothing here has to pick a derivation per layer kind.
    const cluster = clusterLayerOf(colourBy);
    const clusterMeta = cluster ? meta.layers.find((l) => l.name === cluster) : null;
    const rungs = new Set<number>();
    if (cluster) {
      for (const x of artifacts.colourServed) rungs.add(x.rung);
    }
    const levelsServed = [...rungs].sort((x, y) => x - y);
    // The level the colouring is drawn at, which a levelled layer's option shows as chosen: the
    // one chosen, else the explorer's, else the deepest served.
    const drawnLevel = this.level ?? this.autoLevel ?? levelsServed.at(-1) ?? null;
    const selects = this.selectable
      ? html`<div class="selects">
          <div class="col"><span class="xs muted">Colour by</span>
            <select part="select" aria-label="Colour by" @change=${(e: Event) => this.choose((e.target as HTMLSelectElement).value)}>
              <option value="" ?selected=${colourBy === null}>none</option>
              ${colourLayers(meta.layers).flatMap((decl) => {
                // **A levelled layer is offered level by level, by the titles the corpus declared** —
                // Country, Admin 1 … Admin 4 — because that is what the colouring *is* on such a
                // layer: membership at one level. "clusters" named the mechanism, not the data
                // (the owner, 2026-08-28). A layer without levels is offered once, by its title.
                const value = `${CLUSTER_PREFIX}${decl.name}`;
                if (decl.levels.length === 0) {
                  return [html`<option part="cluster-option" value=${value} ?selected=${colourBy === value}>${decl.title || decl.name}</option>`];
                }
                return decl.levels.map(
                  (lv) =>
                    html`<option part="cluster-option" value=${`${value}@${lv.level}`} ?selected=${colourBy === value && (drawnLevel ?? decl.levels.at(-1)!.level) === lv.level}>${lv.title || `level ${lv.level}`}</option>`
                );
              })}
              ${columns.map((c) => html`<option value=${c.name} ?selected=${colourBy === c.name}>${c.name}</option>`)}
            </select></div>
          <div class="col"><span class="xs muted">Layers</span>
            <select part="layers-select" aria-label="Layers" @change=${(e: Event) => this.chooseLayers((e.target as HTMLSelectElement).value)}>
              <option value="" ?selected=${on.length === 0}>${on.length} of ${entries.length} on</option>
              ${entries.map((e) => html`<option value=${e.root.name} ?selected=${on.length === 1 && on[0]!.root.name === e.root.name}>${e.root.name}</option>`)}
            </select></div>
        </div>`
      : nothing;
    const levelSelect =
      this.selectable && cluster && levelsServed.length > 1
        ? html`<div class="col" style="margin-top:8px"><span class="xs muted">Level</span>
            <select part="level-select" aria-label="Level" @change=${(e: Event) => this.chooseLevel((e.target as HTMLSelectElement).value)}>
              <option value="" ?selected=${this.level === null}>${this.autoLevel === null ? 'deepest served' : `auto · ${clusterMeta?.levels.find((x) => x.level === this.autoLevel)?.title ?? `level ${this.autoLevel}`}`}</option>
              ${levelsServed.map((l) => html`<option value=${l} ?selected=${this.level === l}>${clusterMeta?.levels.find((x) => x.level === l)?.title ?? `level ${l}`}</option>`)}
            </select></div>`
        : nothing;
    const heading = this.selectable ? nothing : html`<h2 part="title">Colour</h2>`;
    const swatch = (c: Rgba | readonly number[], text: string, title = '') =>
      html`<div class="row"><span part="swatch" style=${`background:${rgb(c as Rgba)}`}></span><span class="v" title=${title}>${text}</span></div>`;
    const wrap = (body: unknown) => html`<div class="panel">${heading}${selects}${levelSelect}${body}</div>`;
    if (!this.readout && this.selectable) return wrap(html`<span part="state" data-state="shown"></span>`);

    if (colourBy === null) return wrap(html`<span part="state" data-state="shown"></span>`);
    const clusterLayer = clusterLayerOf(colourBy);
    if (clusterLayer) {
      const named = artifacts.colourServed;
      return wrap(html`<span part="state" data-state="shown"></span>
        <div part="swatches">
          ${named.slice(0, 40).map((a) => swatch(artifacts.colours.get(artifacts.table.ordinalOf(a.layer, a.tesseraId)) ?? NEUTRAL, artifactName(a) ?? UNNAMED))}
          ${named.length > 40 ? html`<div class="muted xs">and ${named.length - 40} more</div>` : nothing}
          ${swatch(NEUTRAL, 'not yet known')}
        </div>`);
    }
    const column = columns.find((c) => c.name === colourBy);
    if (!column) return wrap(html`<span part="state" data-state="refused"><span part="refusal">no such column</span></span>`);
    const error = legend.categoryErrors[colourBy];
    if (error) return wrap(html`<span part="state" data-state="refused"><span part="refusal">${error.code}: ${error.detail}</span></span>`);
    if (column.category) {
      const values = legend.categories[colourBy];
      if (!values) return wrap(renderState('loading', s.get('status')));
      const shown = paletteValues(values, legend.ranks[colourBy] ?? {});
      const overflow = values.length - shown.length;
      return wrap(html`<span part="state" data-state="shown"></span>
        <div part="swatches">
          ${shown.map(({value, rank}) => swatch(colourOfRank(rank), value.title && value.title !== value.key ? `${value.key} — ${value.title}` : value.key, `code ${value.code}`))}
          ${overflow > 0 ? swatch(UNMAPPED, `${overflow} rarer value${overflow === 1 ? '' : 's'}`) : nothing}
          ${swatch(UNMAPPED, 'other')}
        </div>`);
    }
    const domain = legend.domains[colourBy];
    if (!domain) return wrap(html`<span part="state" data-state="empty">No values on screen</span>`);
    const stops = Array.from({length: 12}, (_, i) => rgb(colourOfFraction(i / 11))).join(', ');
    const fmt = (n: number) => {
      if (column.arrowType === 'timestamp_us') return new Date(n / 1000).toISOString().slice(0, 10);
      if (Math.abs(n) >= 1e6 || (n !== 0 && Math.abs(n) < 1e-3)) return n.toExponential(2);
      return n.toLocaleString('en-GB');
    };
    return wrap(html`<span part="state" data-state="shown"></span>
      <div part="ramp" style=${`background:linear-gradient(to right, ${stops})`}></div>
      <div class="kv sm"><span part="label">min</span><span part="value" class="v">${fmt(domain.min)}</span><span part="label">max</span><span part="value" class="v">${fmt(domain.max)}</span></div>`);
  }
}

attachContextRoot();
defineOnce('tessera-legend', TesseraLegend);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-legend': TesseraLegend;
  }
}
