import {css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import {type RegionProjection} from '@mosaica/client';
import {MosaicaElement, emit, idString, shapeDetail} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * The selected region: its counts, the held marks inside it as a list, and its actions. *Shown
 * inside* counts the held marks against the matched count. *Matched inside* and *Visible inside*
 * are the store's counts in view over the shape, the figures the status strip shows, exact unless
 * the server answered for a cover of it; before those land, *Visible inside* appears only while no
 * other filter narrows the view. Clicking a listed mark picks it. *Outside* flips the selection to its
 * complement, and *Clear* drops it.
 *
 * A selection is a filter: the map and every count narrow to it once it settles. Not built yet:
 * *Export* and *Save as artifact*, which need server routes; their buttons are disabled with the
 * reason on hover.
 *
 * @summary The selected region's counts, marks and actions.
 * @tagname mosaica-selection
 * @category Elements
 * @fires {CustomEvent<MosaicaEventDetails['mosaica-selectchange']>} mosaica-selectchange - Clear
 *   (with `shape` null) or Outside and Inside (with the new shape and `status` `loading`) was
 *   pressed.
 * @csspart title - The heading, with the shape's kind.
 * @csspart state - The state line, with `data-state`; its tooltip says whether the counts are
 *   exact for the shape or for a cover.
 * @csspart refusal - The words "Selection refused", with `data-code` set to the refusal's code.
 * @csspart counts - The counts.
 * @csspart count-served - The `<mosaica-count>` of marks shown inside.
 * @csspart count-matched - The `<mosaica-count>` matched inside (or outside).
 * @csspart count-visible - The `<mosaica-count>` visible inside (or outside).
 * @csspart label - The heading above the list.
 * @csspart items - The list of held marks inside.
 * @csspart item - One held mark, by `tessera_id`.
 * @csspart actions - The action buttons.
 * @csspart action - One action button.
 */
export class MosaicaSelection extends MosaicaElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: block;
      }
      [part='counts'] {
        margin-bottom: 4px;
      }
      [part='counts'] mosaica-count::part(count) {
        font-weight: 500;
      }
      [part='counts'] mosaica-count::part(label) {
        display: none;
      }
      .items-label {
        margin: 10px 0 4px;
      }
      [part='items'] {
        list-style: none;
        margin: 0;
        padding: 0;
        max-height: 160px;
        overflow-y: auto;
      }
      [part='item'] {
        min-height: 26px;
        font-family: var(--_mosaica-font-mono);
        font-size: 12px;
      }
      [part='actions'] {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
        margin-top: 12px;
      }
    `
  ];

  /** The region to show in place of the store's `region` projection. */
  @property({attribute: false}) accessor region: RegionProjection | null = null;

  private get shown(): RegionProjection | null {
    return this.region ?? this.resolvedStore?.get('region') ?? null;
  }

  override render(): TemplateResult | typeof nothing {
    const r = this.shown;
    const heading = (shape: string = '') => html`<h2 part="title">Selection<span class="summary">${shape}</span></h2>`;
    if (!r) return html`<div class="panel">${heading()}<span part="state" data-state="detached"></span></div>`;
    const stale = this.resolvedStore?.get('status').stale ?? false;
    const state = r.status === 'shown' ? (stale ? 'stale' : 'shown') : r.status;
    const kv = (label: string, count: unknown) => html`<div class="k">${label}</div><div class="v">${count}</div>`;
    const counts =
      r.status === 'shown'
        ? html`<div part="counts" class="kv">
            ${kv('Shown inside', html`<mosaica-count part="count-served" .count=${r.served} .stale=${stale} figure="shown"></mosaica-count>`)}
            ${kv(r.shape.outside ? 'Matched outside' : 'Matched inside', html`<mosaica-count part="count-matched" .masked=${r.matched} .stale=${stale}></mosaica-count>`)}
            ${r.visible ? kv(r.shape.outside ? 'Visible outside' : 'Visible inside', html`<mosaica-count part="count-visible" .masked=${r.visible} .stale=${stale}></mosaica-count>`) : nothing}
          </div>`
        : nothing;
    const stateRegion =
      r.status === 'loading'
        ? html`<span part="state" data-state="loading"><span class="dot quiet"></span>Counting<span class="skel" aria-hidden="true"></span></span>`
        : r.status === 'refused'
          ? html`<span part="state" data-state="refused"><span class="dot refuse"></span><span part="refusal" data-code=${r.refusal?.code ?? nothing}>Selection refused</span></span>`
          : html`<span part="state" data-state=${state} title=${r.verdict === null ? 'Not counted yet' : r.verdict.exact ? 'Exact for the shape' : 'Approximate: counted over cells around the shape'}>${stale ? html`<span class="dot warn"></span>Data updated` : nothing}</span>`;
    const ids = Array.from(r.held.ids, idString);
    const waiting = (label: unknown, reason: string) => html`<button part="action" class="btn off" type="button" disabled title=${reason}>${label}</button>`;
    return html`<div class="panel">${heading(r.shape.outside ? `outside ${r.shape.kind}` : r.shape.kind)}${stateRegion}${counts}
      ${ids.length > 0
        ? html`<div part="label" class="xs muted items-label">Shown items</div>
          <ul part="items" class="list" aria-label="held marks inside the selection">
            ${ids.map(
              (id) => html`<li part="item" class="item" tabindex="0" role="button"
                  @click=${() => void this.resolvedStore?.pick(BigInt(id))}
                  @keydown=${(e: KeyboardEvent) => {
                    if (e.key === 'Enter' || e.key === ' ') void this.resolvedStore?.pick(BigInt(id));
                  }}><span class="name">${id}</span></li>`
            )}
            ${r.held.count > ids.length ? html`<li class="muted sm">${(r.held.count - ids.length).toLocaleString('en-GB')} more not listed</li>` : nothing}
          </ul>`
        : nothing}
      <div part="actions">
        <button part="action" class="btn" type="button" @click=${() => {
          this.resolvedStore?.select(null);
          emit(this, 'mosaica-selectchange', {shape: null});
        }}>${icon('close', 13)}Clear</button>
        <button part="action" class="btn" type="button" title=${r.shape.outside ? 'Filter to the inside of the shape' : 'Filter to the outside of the shape'} @click=${() => {
          const next = {...r.shape, outside: !r.shape.outside};
          this.resolvedStore?.select(next);
          emit(this, 'mosaica-selectchange', {shape: shapeDetail(next), status: 'loading'});
        }}>${icon(r.shape.outside ? 'filter' : 'outside', 13)}${r.shape.outside ? 'Inside' : 'Outside'}</button>
        ${waiting('Export', 'Not available yet')}
        ${waiting('Save as artifact', 'Not available yet')}
      </div>
    </div>`;
  }
}

attachContextRoot();
defineOnce('mosaica-selection', MosaicaSelection);

declare global {
  interface HTMLElementTagNameMap {
    'mosaica-selection': MosaicaSelection;
  }
}
