import {html, nothing, type TemplateResult} from 'lit';
import type {StatusProjection} from '@tesseradb/client';

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
  /** The Refresh button, present when stale. */
  onRefresh?: (() => void) | null;
  /** The Retry button, present when refused. */
  onRetry?: (() => void) | null;
  /** The Sign in button, shown on expiry when the host gave the element a renewal. */
  onReauthorise?: (() => void) | null;
  /** Whether the panel has shown nothing yet, which loading then names Loading rather than Updating. */
  first?: boolean;
};

/**
 * The words for a state as the status strip says it: two or three plain words. What went wrong in
 * detail stays in `status.refusal`, which a host reads, and is not put on screen.
 */
export function stateWord(state: PanelState, status: StatusProjection | null | undefined): string {
  switch (state) {
    case 'detached':
      return 'Not connected';
    case 'loading':
      return status && !status.sessionWarm ? 'Connecting' : 'Updating';
    case 'retrying':
      return 'Reconnecting';
    case 'shown':
      return 'Up to date';
    case 'empty':
      return 'Nothing in view';
    case 'refused':
      return 'View refused';
    case 'expired':
      return 'Session expired';
    case 'stale':
      return 'Data updated';
  }
}

/** The refusal's words, with its code on `data-code` for a host's rules and tests. */
export function refusalText(words: string, code: string | null | undefined): TemplateResult {
  return html`<span part="refusal" data-code=${code ?? nothing}>${words}</span>`;
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
    case 'loading': {
      const words = actions.first && status?.sessionWarm ? 'Loading' : stateWord(state, status);
      return html`<span part="state" data-state="loading"><span class="dot quiet"></span>${words}${status && !status.sessionWarm ? nothing : html`<span class="skel" aria-hidden="true"></span>`}</span>`;
    }
    case 'retrying':
      return html`<span part="state" data-state="retrying"><span class="dot warn"></span>${stateWord(state, status)}</span>`;
    case 'shown':
      return html`<span part="state" data-state="shown"></span>`;
    case 'empty':
      return html`<span part="state" data-state="empty">${stateWord(state, status)}</span>`;
    case 'refused':
      return html`<span part="state" data-state="refused"><span class="dot refuse"></span>${refusalText(stateWord(state, status), refusal?.code)}${actions.onRetry
          ? html`<button part="retry" class="btn small" type="button" @click=${actions.onRetry}>Retry</button>`
          : nothing}</span>`;
    case 'expired':
      return html`<span part="state" data-state="expired"><span class="dot refuse"></span>${stateWord(state, status)}${actions.onReauthorise
          ? html`<button part="reauthorise" class="btn small" type="button" @click=${actions.onReauthorise}>Sign in</button>`
          : nothing}</span>`;
    case 'stale':
      return html`<span part="state" data-state="stale"><span class="dot warn"></span>${stateWord(state, status)}<button part="refresh" class="btn small" type="button" @click=${actions.onRefresh ?? undefined}>Refresh</button></span>`;
  }
}
