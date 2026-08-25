import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {CLUSTER_PREFIX, NEUTRAL, type Rgba} from '@tesseradb/client';
import {UNMAPPED, artifactName, clusterLayerOf, colourOfFraction, colourOfRank, css as rgb, paletteValues} from '@tesseradb/deck';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, stateOf} from './states.js';
import {chrome, tokens} from './tokens.js';

/**
 * `<tessera-legend>` — what the colours mean (design §5.3 tier 2): the values marks on screen
 * carry, resolved per column and never per vocabulary; a numeric domain as a ramp, derived from
 * the marks served and never a corpus-wide range; under cluster colour, the served artifacts in
 * their colours. `selectable` adds the colour-by selector, which offers the rendered columns and
 * *cluster* per layer that is on — exact only (decision 0099): a point wears a cluster's colour
 * only because the wire named it a member.
 */
export class TesseraLegend extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
        padding: var(--tessera-space) calc(var(--tessera-space) * 1.6);
        background: var(--tessera-panel-bg);
        border: 1px solid var(--tessera-border);
        border-radius: var(--tessera-radius);
        max-width: 260px;
      }
      select {
        width: 100%;
        margin-bottom: var(--tessera-space);
      }
      [part='swatches'] {
        max-height: 180px;
        overflow-y: auto;
      }
      [part='swatch'] {
        display: inline-block;
        width: 10px;
        height: 10px;
        border-radius: 2px;
        margin-right: 6px;
        vertical-align: middle;
      }
      [part='ramp'] {
        height: 8px;
        border-radius: var(--tessera-radius);
        margin: 4px 0;
      }
      .v {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
    `
  ];

  @property({type: Boolean}) accessor selectable = false;

  private choose(value: string): void {
    const s = this.resolvedStore;
    if (!s) return;
    const chosen = value === '' ? null : value;
    s.setColourBy(chosen);
    emit(this, 'tessera-colourchange', {colourBy: chosen});
  }

  override render() {
    const s = this.resolvedStore;
    const heading = html`<h2 part="title">Colour</h2>`;
    const meta = s?.get('meta') ?? null;
    if (!s || !meta) return html`${heading}${renderState(stateOf(s?.get('status')), s?.get('status'))}`;
    const legend = s.get('legend');
    const artifacts = s.get('artifacts');
    const columns = meta.declaredScalars.filter((c) => c.render);
    const colourBy = legend.colourBy;
    const select = this.selectable
      ? html`<select part="select" aria-label="colour by" @change=${(e: Event) => this.choose((e.target as HTMLSelectElement).value)}>
          <option value="" ?selected=${colourBy === null}>uniform</option>
          ${columns.map((c) => html`<option value=${c.name} ?selected=${colourBy === c.name}>${c.name} · ${c.category ? 'category' : c.arrowType}</option>`)}
          ${artifacts.layers.map((l) => {
            const value = `${CLUSTER_PREFIX}${l}`;
            return html`<option part="cluster-option" value=${value} ?selected=${colourBy === value}>cluster · ${meta.layers.find((x) => x.name === l)?.title || l}</option>`;
          })}
        </select>`
      : nothing;
    const swatch = (c: Rgba | readonly number[], text: string, title = '') =>
      html`<div class="row"><span><span part="swatch" style=${`background:${rgb(c as Rgba)}`}></span><span class="v" title=${title}>${text}</span></span></div>`;

    if (colourBy === null) {
      return html`${heading}${select}<span part="state" data-state="shown"></span><div class="muted">every mark one colour — pick a column, or a cluster layer that is on</div>`;
    }
    const clusterLayer = clusterLayerOf(colourBy);
    if (clusterLayer) {
      const named = artifacts.served.filter((a) => a.layer === clusterLayer);
      return html`${heading}${select}
        <span part="state" data-state="shown"></span>
        <div part="swatches">
          ${named.slice(0, 40).map((a) => swatch(artifacts.colours.get(artifacts.table.ordinalOf(a.layer, a.tesseraId)) ?? NEUTRAL, artifactName(a)))}
          ${named.length > 40 ? html`<div class="muted">…and ${named.length - 40} more served</div>` : nothing}
          ${swatch(NEUTRAL, 'not known here yet')}
        </div>
        <div class="muted">exact only: a point wears a cluster's colour because the wire named it a member; neutral is not a guess. ${artifacts.coverage.stale > 0 ? `refreshing ${artifacts.coverage.stale} tiles` : 'colours exact'}</div>`;
    }
    const column = columns.find((c) => c.name === colourBy);
    if (!column) return html`${heading}${select}<span part="state" data-state="refused"><span part="refusal">no such rendered column</span></span>`;
    const error = legend.categoryErrors[colourBy];
    if (error) {
      return html`${heading}${select}<span part="state" data-state="refused"><span class="badge">refused</span><span part="refusal">${error.code}: ${error.detail}</span></span>
        <div class="muted">drawn unmapped — every served mark is still on the map, only its value is unnamed</div>`;
    }
    if (column.category) {
      const values = legend.categories[colourBy];
      if (!values) return html`${heading}${select}${renderState('loading', s.get('status'))}`;
      const shown = paletteValues(values, legend.ranks[colourBy] ?? {});
      const overflow = values.length - shown.length;
      return html`${heading}${select}
        <span part="state" data-state="shown"></span>
        <div part="swatches">
          ${shown.map(({value, rank}) => swatch(colourOfRank(rank), value.title && value.title !== value.key ? `${value.key} — ${value.title}` : value.key, `code ${value.code}`))}
          ${overflow > 0 ? swatch(UNMAPPED, `${overflow} rarer value${overflow === 1 ? '' : 's'}`) : nothing}
          ${swatch(UNMAPPED, 'absent / unresolved')}
        </div>
        <div class="muted">values on screen, not the whole vocabulary</div>`;
    }
    const domain = legend.domains[colourBy];
    if (!domain) return html`${heading}${select}<span part="state" data-state="empty"><span class="muted">no numeric values on screen</span></span>`;
    const stops = Array.from({length: 12}, (_, i) => rgb(colourOfFraction(i / 11))).join(', ');
    const fmt = (n: number) => {
      if (column.arrowType === 'timestamp_us') return new Date(n / 1000).toISOString().slice(0, 10);
      if (Math.abs(n) >= 1e6 || (n !== 0 && Math.abs(n) < 1e-3)) return n.toExponential(2);
      return n.toLocaleString('en-GB');
    };
    return html`${heading}${select}
      <span part="state" data-state="shown"></span>
      <div part="ramp" style=${`background:linear-gradient(to right, ${stops})`}></div>
      <div class="row"><span part="label">min</span><span part="value">${fmt(domain.min)}</span></div>
      <div class="row"><span part="label">max</span><span part="value">${fmt(domain.max)}</span></div>
      <div class="muted">range of marks served, not of the corpus</div>`;
  }
}

attachContextRoot();
defineOnce('tessera-legend', TesseraLegend);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-legend': TesseraLegend;
  }
}
