import {css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {NO_COUNT, NO_MASKED, type StatusProjection, type ViewProjection} from '@tesseradb/client';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {icon} from './icons.js';
import {renderState, showsContent, stateOf, stateWord, type PanelState} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * `<tessera-status>` — the state and the three counts, as one line (design §5.3, decision 0098),
 * drawn as the boards draw it (`StatusStates.png`): a dot and a word, then three cells —
 * *4,812 shown · 12,465 matched · 181,900 visible* — in that order because their relationship is
 * the content, with a fourth cell — *the highlight matched N* — where the request carried a
 * `highlight` (`highlight-and-hierarchy.md` §5.2, the owner's words); it is absent where none is
 * set, because `highlighted` equals `matched` then and a cell repeating a number says there is
 * a second answer where there is not; skeleton bars in the cells while loading or retrying; *Starting session…* on the
 * first request; *Corpus updated* with the counts dimmed and *Refresh* when stale; *Session
 * expired · Sign in again*. The detail behind the numbers is a hover; `expanded` renders it as
 * a card.
 *
 * An `aria-live` region, so a refusal, an expiry or a stale signal is announced (§5.8). Fires
 * `tessera-statechange` on every transition of §5.4 and `tessera-expired` once per expiry.
 */
export class TesseraStatus extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: inline-block;
      }
      [part='strip'] {
        display: inline-flex;
        align-items: stretch;
        background: var(--tessera-surface);
        border: 1px solid var(--tessera-line);
        border-radius: var(--tessera-radius);
        box-shadow: var(--tessera-shadow);
        height: 36px;
        font-size: 12px;
        white-space: nowrap;
      }
      [part='strip'] > * {
        display: flex;
        align-items: center;
        gap: 6px;
        padding: 0 12px;
      }
      [part='strip'] > * + * {
        border-left: 1px solid var(--tessera-line-2);
      }
      [part='state'] {
        font-weight: 600;
        color: var(--tessera-ink);
        font-size: 12px;
      }
      [part='state'][data-state='loading'],
      [part='state'][data-state='retrying'] {
        color: var(--tessera-ink-2);
      }
      [part='state'][data-state='loading'] .dot,
      [part='state'][data-state='retrying'] .dot {
        background: var(--tessera-ink-3);
      }
      [part='state'][data-state='retrying'] .dot {
        background: var(--tessera-warn);
      }
      [part='state'][data-state='refused'],
      [part='state'][data-state='expired'] {
        background: var(--tessera-refuse-soft);
        color: var(--tessera-refuse);
      }
      [part='state'][data-state='stale'] {
        background: var(--tessera-warn-soft);
        color: var(--tessera-warn);
      }
      [part='state'] .skel {
        display: none;
      }
      [part='state'] .btn {
        height: 26px;
        padding: 0 10px;
        margin-left: 4px;
        font-size: 12px;
        font-weight: 600;
        border-radius: 3px;
      }
      [part='state'][data-state='expired'] .btn,
      [part='state'][data-state='stale'] .btn {
        margin: 0 -6px 0 6px;
      }
      [part='refusal'] {
        font-weight: 400;
        color: var(--tessera-ink-2);
        padding-left: 12px;
        margin-left: 12px;
        border-left: 1px solid var(--tessera-line-2);
        height: 100%;
        display: flex;
        align-items: center;
      }
      .cell {
        gap: 5px;
      }
      .cell .l {
        color: var(--tessera-ink-2);
      }
      .cell.dim {
        opacity: 0.45;
      }
      .cell tessera-count::part(count) {
        font-size: 12.5px;
      }
      .cell tessera-count::part(label) {
        font-weight: 400;
      }
      [part='card'] {
        margin-top: 8px;
        padding: 10px 12px;
        background: var(--tessera-surface);
        border: 1px solid var(--tessera-line);
        border-radius: var(--tessera-radius);
        box-shadow: var(--tessera-shadow);
        font-size: 12px;
      }
    `
  ];

  @property({type: Boolean}) accessor expanded = false;
  /** The host's renewal, shown on expiry beside the prompt. */
  @property({attribute: false}) accessor reauthorise: (() => void) | null = null;

  private lastState: PanelState | null = null;
  private expiryFired = false;

  private get status(): StatusProjection | null {
    return this.resolvedStore?.get('status') ?? null;
  }

  private get view(): ViewProjection | null {
    return this.resolvedStore?.get('view') ?? null;
  }

  protected override updated(): void {
    const state = stateOf(this.status);
    if (state !== this.lastState) {
      const from = this.lastState;
      this.lastState = state;
      emit(this, 'tessera-statechange', {from, to: state});
    }
    if (state === 'expired' && !this.expiryFired) {
      this.expiryFired = true;
      emit(this, 'tessera-expired', {refusal: this.status?.refusal ?? null});
    } else if (state !== 'expired') {
      this.expiryFired = false;
    }
  }

  private hover(): string {
    const v = this.view;
    const r = this.resolvedStore?.get('replica');
    if (!v || !r) return '';
    const parts = [`depth ${v.depth}`];
    if (v.provisional > 0) parts.push(`${v.provisional.toLocaleString('en-GB')} provisional`);
    const a = this.resolvedStore?.get('artifacts');
    if (a && a.layers.length > 0 && a.status === 'shown') {
      parts.push(a.coverage.stale === 0 ? 'colours exact' : `refreshing ${a.coverage.stale.toLocaleString('en-GB')} tiles`);
    }
    parts.push(`replica ${(r.bytes / 1e6).toFixed(1)} MB · ${r.bands.toLocaleString('en-GB')} bands`);
    return parts.join(' · ');
  }

  override render() {
    const status = this.status;
    const state = stateOf(status);
    const stale = status?.stale ?? false;
    const v = this.view;
    // Stale keeps the strip's shape — three dimmed cells — and no number: the numbers on screen
    // were drawn against a corpus that has since moved (the formatter renders nothing).
    const content = state === 'shown' && v;
    const skeleton = (state === 'loading' && status?.sessionWarm !== false) || state === 'retrying' || state === 'stale';
    const cell = (label: string, inner: unknown, dim = false) => html`<div class=${`cell${dim ? ' dim' : ''}`}>${inner}<span class="l">${label}</span></div>`;
    const cells = content
      ? html`<div class="cell"><tessera-count part="count-shown" .count=${v.served} .stale=${stale} figure="shown" label="shown"></tessera-count></div>
          <div class="cell"><tessera-count part="count-matched" .masked=${v.matched} .stale=${stale} label="matched"></tessera-count></div>
          ${v.highlighting
            ? html`<div class="cell"><tessera-count part="count-highlighted" .masked=${v.highlighted} .stale=${stale} label="the highlight matched"></tessera-count></div>`
            : nothing}
          <div class="cell"><tessera-count part="count-visible" .masked=${v.visible} .stale=${stale} label="visible"></tessera-count></div>`
      : skeleton
        ? html`${cell('shown', html`<span class="skel" aria-hidden="true"></span>`, stale)}${cell('matched', html`<span class="skel" aria-hidden="true"></span>`, stale)}${cell('visible', html`<span class="skel" aria-hidden="true"></span>`, stale)}`
        : nothing;
    const empty = state === 'empty' ? html`<div class="cell"><span class="l">nothing in this region</span></div>` : nothing;
    // Stale keeps its numbers beside the word, so the strip reads Corpus updated · … · Refresh.
    const strip =
      state === 'stale'
        ? html`<span part="state" data-state="stale">${icon('clock', 14)}${stateWord(state, status)}</span>${cells}
            <div><button part="refresh" class="btn primary" type="button" @click=${() => this.resolvedStore?.refresh()}>${icon('refresh', 13)}Refresh</button></div>`
        : state === 'shown' || state === 'empty'
          ? html`<span part="state" data-state=${state}><span class="dot"></span>${stateWord(state, status)}</span>${cells}${empty}`
          : html`${renderState(state, status, {onRefresh: () => this.resolvedStore?.refresh(), onReauthorise: this.reauthorise})}${cells}`;
    return html`<div part="strip" role="status" aria-live="polite" title=${this.hover()}>${strip}</div>
      ${this.expanded && showsContent(state) && v ? this.card(v, stale) : nothing}`;
  }

  private card(v: ViewProjection, stale: boolean) {
    const r = this.resolvedStore?.get('replica');
    const row = (label: string, value: unknown) => html`<div class="k">${label}</div><div class="v">${value}</div>`;
    return html`<div part="card"><div class="kv">
      ${row('Shown', html`<tessera-count .count=${v.served ?? NO_COUNT} .stale=${stale}></tessera-count>`)}
      ${row('Matched by filters', html`<tessera-count .masked=${v.matched ?? NO_MASKED} .stale=${stale}></tessera-count>`)}
      ${v.highlighting ? row('The highlight matched', html`<tessera-count .masked=${v.highlighted ?? NO_MASKED} .stale=${stale}></tessera-count>`) : nothing}
      ${row('Visible to you here', html`<tessera-count .masked=${v.visible ?? NO_MASKED} .stale=${stale}></tessera-count>`)}
      ${row('Region', `depth ${v.depth}`)}
      ${row('Provisional marks', v.provisional.toLocaleString('en-GB'))}
      ${r ? row('Replica', `${(r.bytes / 1e6).toFixed(1)} MB · ${r.points.toLocaleString('en-GB')} points · ${r.bands.toLocaleString('en-GB')} bands`) : nothing}
    </div></div>`;
  }
}

attachContextRoot();
defineOnce('tessera-status', TesseraStatus);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-status': TesseraStatus;
  }
}
