import {css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import {NO_COUNT, NO_MASKED, type StatusProjection, type ViewProjection} from '@tesseradb/client';
import {TesseraElement, emit} from './base.js';
import {attachContextRoot, defineOnce} from './define.js';
import {renderState, showsContent, stateOf, type PanelState} from './states.js';
import {chrome, tokens} from './tokens.js';
import './count.js';

/**
 * The view's state and counts on one line: `● | 16,822,190 of 21,406,522 match | 5,390 shown`.
 * While a highlight is set, a cell reads `N highlighted of M`, M being the matched count, and the
 * match cell is left out unless a filter or selection narrows what matches. While the view is up to
 * date the first cell is a dot alone, titled "Up to date"; otherwise it names the state in two or
 * three words (Updating, Reconnecting, Data updated, View refused, Session expired) with the action
 * the state offers: Refresh when the data changed, Retry when the view was refused, and Sign in on
 * expiry where `reauthorise` is set. The counts grey out while they are not current. The strip
 * sizes to its content and does not wrap. `compact` shortens the figures (`16.8M of 21.4M match`)
 * and drops the shown count. `expanded` renders the figures again as a card below the strip.
 *
 * The strip is an `aria-live` region, so a refusal, an expiry or a change of data is announced. The
 * state is one of the eight panel states (see `PanelState`); what was refused and why stays in the
 * store's `status.refusal`, and the refusal's code is on the refusal part's `data-code`.
 *
 * @summary The state and the counts of the view, on one line.
 * @tagname tessera-status
 * @category Elements
 * @fires {CustomEvent<TesseraEventDetails['tessera-statechange']>} tessera-statechange - On every
 *   change of panel state, including the first render.
 * @fires {CustomEvent<TesseraEventDetails['tessera-expired']>} tessera-expired - Once each time the
 *   session expires.
 * @csspart strip - The one-line strip.
 * @csspart state - The state's dot and words, with `data-state` set to the panel state.
 * @csspart refusal - The words "View refused", with `data-code` set to the refusal's code.
 * @csspart refresh - The Refresh button, when the data changed under the view.
 * @csspart retry - The Retry button, when the view was refused.
 * @csspart reauthorise - The Sign in button, on expiry where `reauthorise` is set.
 * @csspart count-shown - The `<tessera-count>` of marks shown.
 * @csspart count-matched - The `<tessera-count>` matched by the filters.
 * @csspart count-highlighted - The `<tessera-count>` the highlight matched, while a highlight is set.
 * @csspart count-of - The matched count the highlighted count is out of, while a highlight is set.
 * @csspart count-visible - The `<tessera-count>` of items the viewer may see here, which the matched
 *   count is out of.
 * @csspart card - The card of figures, under `expanded`.
 */
export class TesseraStatus extends TesseraElement {
  static override styles = [
    tokens,
    chrome,
    css`
      :host {
        display: inline-block;
        max-width: 100%;
        vertical-align: top;
      }
      [part='strip'] {
        display: inline-flex;
        align-items: stretch;
        max-width: 100%;
        height: 32px;
        overflow: hidden;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
        font-size: 12px;
        color: var(--_tessera-ink-2);
        font-variant-numeric: tabular-nums;
        white-space: nowrap;
      }
      [part='strip'][data-state='detached'] {
        display: none;
      }
      .cell {
        display: flex;
        align-items: center;
        gap: 4px;
        padding: 0 12px;
        flex: none;
      }
      .cell + .cell {
        border-left: 1px solid var(--_tessera-line-2);
      }
      .cell.dim {
        opacity: 0.4;
      }
      .cell.state {
        padding: 0 5px 0 12px;
      }
      .cell.state.bare {
        padding: 0 12px;
      }
      [part='state'] {
        gap: 7px;
        color: inherit;
      }
      [part='state'] .skel {
        display: none;
      }
      [part='state'] .btn {
        margin-left: 1px;
      }
      [part='state']:not(:has(.btn)) {
        padding-right: 7px;
      }
      .cell tessera-count::part(count) {
        font-weight: 600;
      }
      .cell tessera-count[part~='count-visible']::part(count) {
        color: inherit;
        font-weight: 400;
      }
      .cell tessera-count[part~='count-shown']::part(count) {
        color: inherit;
        font-weight: 400;
      }
      .cell tessera-count[part~='count-of']::part(count) {
        color: inherit;
        font-weight: 400;
      }
      .cell tessera-count.lit::part(count) {
        color: var(--_tessera-highlight);
      }
      .cell tessera-count::part(label) {
        margin-left: 0.3em;
      }
      :host([compact]) .cell + .cell {
        border-left: 0;
        padding-left: 0;
      }
      :host([compact]) .cell.state {
        padding-right: 7px;
      }
      [part='card'] {
        margin-top: 8px;
        padding: 10px 12px;
        background: var(--_tessera-surface);
        border: 1px solid var(--_tessera-line);
        border-radius: var(--_tessera-radius);
        box-shadow: var(--_tessera-shadow);
        font-size: 12px;
      }
    `
  ];

  /** Renders the figures again as a card below the strip. */
  @property({type: Boolean}) accessor expanded = false;
  /** Shortens the figures to one decimal (`16.8M`) and drops the shown count, where room is short. */
  @property({type: Boolean, reflect: true}) accessor compact = false;
  /** Called by the Sign in button that the expired state shows; without it, no button. */
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

  override render(): TemplateResult | typeof nothing {
    const status = this.status;
    const state = stateOf(status);
    const stale = status?.stale ?? false;
    const v = this.view;
    const refresh = () => this.resolvedStore?.refresh();
    const first =
      state === 'shown'
        ? html`<div class="cell state bare"><span part="state" data-state="shown"><span class="dot" role="img" aria-label="Up to date" title="Up to date"></span></span></div>`
        : html`<div class="cell state">${renderState(state, status, {onRefresh: refresh, onRetry: refresh, onReauthorise: this.reauthorise})}</div>`;
    return html`<div part="strip" role="status" aria-live="polite" data-state=${state}>${first}${this.counts(state, v, stale)}</div>
      ${this.expanded && showsContent(state) && v ? this.card(v, stale) : nothing}`;
  }

  /**
   * The count cells: current in the shown state, greyed out while a new answer is awaited or the
   * last one was refused, and skeletons where there is no answer yet. Nothing where there is no
   * view to count, or where the view is empty.
   */
  private counts(state: PanelState, v: ViewProjection | null, stale: boolean): TemplateResult | typeof nothing {
    if (state === 'detached' || state === 'empty' || !v) return nothing;
    if (state === 'loading' && this.status?.sessionWarm === false) return nothing;
    const dim = state !== 'shown';
    const answered = v.matched.exact || v.matched.value > 0 || v.visible.value > 0;
    const cls = `cell${dim ? ' dim' : ''}`;
    if (!answered || stale) {
      const skel = html`<span class="skel" aria-hidden="true"></span>`;
      return html`<div class=${cls}>${skel}<span>match</span></div>${this.compact ? nothing : html`<div class=${cls}>${skel}<span>shown</span></div>`}`;
    }
    // Under a highlight with nothing narrowing, the match cell would read "M of M match" beside
    // the highlight's, so the highlight's cell stands alone and names M.
    const narrowed = this.resolvedStore?.requestFilters() != null;
    const match = html`<div class=${cls}>
      <tessera-count part="count-matched" .masked=${v.matched} .compact=${this.compact}></tessera-count><span>of</span><tessera-count
        part="count-visible" .masked=${v.visible} .compact=${this.compact}></tessera-count><span>match</span>
    </div>`;
    return html`${v.highlighting && !narrowed ? nothing : match}
      ${v.highlighting
        ? html`<div class=${cls}><tessera-count part="count-highlighted" class="lit" .masked=${v.highlighted} .compact=${this.compact}></tessera-count><span>highlighted of</span><tessera-count
              part="count-of" .masked=${v.matched} .compact=${this.compact}></tessera-count></div>`
        : nothing}
      ${this.compact || !v.served.exact ? nothing : html`<div class=${cls}><tessera-count part="count-shown" .count=${v.served} figure="shown" label="shown"></tessera-count></div>`}`;
  }

  private card(v: ViewProjection, stale: boolean) {
    const row = (label: string, value: unknown) => html`<div class="k">${label}</div><div class="v">${value}</div>`;
    return html`<div part="card"><div class="kv">
      ${row('Shown', html`<tessera-count .count=${v.served ?? NO_COUNT} .stale=${stale}></tessera-count>`)}
      ${row('Match the filters', html`<tessera-count .masked=${v.matched ?? NO_MASKED} .stale=${stale}></tessera-count>`)}
      ${v.highlighting ? row('Highlighted', html`<tessera-count .masked=${v.highlighted ?? NO_MASKED} .stale=${stale}></tessera-count>`) : nothing}
      ${row('In this view', html`<tessera-count .masked=${v.visible ?? NO_MASKED} .stale=${stale}></tessera-count>`)}
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
