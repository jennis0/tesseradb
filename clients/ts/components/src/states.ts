import {html, nothing, type TemplateResult} from 'lit';
import type {StatusProjection} from '@tesseradb/client';
import {icon} from './icons.js';

/**
 * The eight panel states. Every panel shows its state in one `part="state"` region, with
 * `data-state` set to it, so one rule restyles every panel. Each follows from the store's `status`
 * projection:
 *
 * | state | from `status` |
 * |---|---|
 * | detached | no store |
 * | loading | `idle` or `loading` |
 * | retrying | `retrying` |
 * | shown | `shown` |
 * | empty | `empty` |
 * | refused | `refused` |
 * | expired | the `expired` flag: `expired-token`, a `bad-credential` on a token this store used, or `expiresAt` passed |
 * | stale | `shown` with `stale` |
 *
 * `expired` is a named refusal and wins over `refused`. `stale` is `shown` with the picture older
 * than the numbers.
 */
export type PanelState = 'detached' | 'loading' | 'retrying' | 'shown' | 'empty' | 'refused' | 'expired' | 'stale';

export function stateOf(status: StatusProjection | null | undefined): PanelState {
  if (!status) return 'detached';
  if (status.expired) return 'expired';
  switch (status.status) {
    case 'idle':
    case 'loading':
      return 'loading';
    case 'retrying':
      return 'retrying';
    case 'empty':
      return 'empty';
    case 'refused':
      return 'refused';
    case 'shown':
      return status.stale ? 'stale' : 'shown';
  }
}

/** Whether a panel in this state shows its content: the numbers, the list, the card. */
export function showsContent(state: PanelState): boolean {
  return state === 'shown' || state === 'stale';
}

export type StateActions = {
  /** The refresh control, present when stale. */
  onRefresh?: (() => void) | null;
  /** Renewal, shown on expiry when the host gave the map an `authorise` property. */
  onReauthorise?: (() => void) | null;
};

/** The word for a state, as the strip's badge says it. */
export function stateWord(state: PanelState, status: StatusProjection | null | undefined): string {
  switch (state) {
    case 'detached':
      return 'No data';
    case 'loading':
      return status && !status.sessionWarm ? 'Starting session…' : 'Loading';
    case 'retrying':
      return 'Retrying';
    case 'shown':
    case 'empty':
      return 'Current';
    case 'refused':
      return 'Refused';
    case 'expired':
      return 'Session expired';
    case 'stale':
      return 'Corpus updated';
  }
}

/**
 * The state region every panel renders: a `part="state"` carrying `data-state`, and the line the
 * state needs, such as a skeleton while loading or the refresh control when stale.
 */
export function renderState(
  state: PanelState,
  status: StatusProjection | null | undefined,
  actions: StateActions = {}
): TemplateResult | typeof nothing {
  const refusal = status?.refusal ?? null;
  switch (state) {
    case 'detached':
      // Empty: "empty" and "refused" are answers, and a detached panel has none.
      return html`<span part="state" data-state="detached"></span>`;
    case 'loading':
      return html`<span part="state" data-state="loading"><span class="dot quiet"></span>${stateWord(state, status)}${status && !status.sessionWarm ? nothing : html`<span class="skel" aria-hidden="true"></span>`}</span>`;
    case 'retrying':
      return html`<span part="state" data-state="retrying"><span class="dot warn"></span>Retrying<span class="skel" aria-hidden="true"></span></span>`;
    case 'shown':
      return html`<span part="state" data-state="shown"></span>`;
    case 'empty':
      return html`<span part="state" data-state="empty">Nothing here</span>`;
    case 'refused':
      return html`<span part="state" data-state="refused">${icon('warn', 14)}Refused<span part="refusal" class="mono">${refusal ? `${refusal.code}${refusal.detail ? ' · ' + refusal.detail : ''}` : ''}</span></span>`;
    case 'expired':
      return html`<span part="state" data-state="expired">${icon('lock', 14)}Session expired${actions.onReauthorise
          ? html`<button part="reauthorise" class="btn primary" type="button" @click=${actions.onReauthorise}>Sign in again</button>`
          : nothing}</span>`;
    case 'stale':
      return html`<span part="state" data-state="stale">${icon('clock', 14)}Corpus updated<button part="refresh" class="btn primary" type="button" @click=${actions.onRefresh ?? undefined}>${icon('refresh', 13)}Refresh</button></span>`;
  }
}
