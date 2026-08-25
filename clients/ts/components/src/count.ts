import {LitElement, css, html, nothing} from 'lit';
import {property} from 'lit/decorators.js';
import {formatCount, formatMasked, type Count, type Masked} from '@tesseradb/client';
import {attachContextRoot, defineOnce} from './define.js';
import {tokens} from './tokens.js';

/**
 * `<tessera-count>` — renders a `Count` or a `Masked` correctly and nothing else (design §5.3
 * tier 3, §4). Both figures or neither for a sample; one figure or none for a scalar, an inexact
 * one marked approximate; neither against a stale view. The rule is the formatter's
 * (`@tesseradb/client`), so a host writing its own status line uses this and gets it for free —
 * "12,040 of 12,040" against a cluster is false, not secret, and this element cannot render it.
 *
 * `part="count"` carries `data-kind` (`sample` or `scalar`), `data-exact`, and `data-empty` when
 * the rule rendered nothing, so the acceptance harness can read the decision and not just the
 * text.
 */
export class TesseraCount extends LitElement {
  static override styles = [
    tokens,
    css`
      :host {
        display: inline;
      }
      [part='count'] {
        color: var(--tessera-fg-strong);
        font-variant-numeric: tabular-nums;
      }
      [part='count'][data-exact='false'] {
        color: var(--tessera-fg);
      }
      [part='label'] {
        color: var(--tessera-fg-muted);
        margin-left: 0.4em;
      }
    `
  ];

  @property({attribute: false}) accessor count: Count | null = null;
  @property({attribute: false}) accessor masked: Masked | null = null;
  @property({type: Boolean}) accessor stale = false;
  @property() accessor label = '';

  override render() {
    const kind = this.count ? 'sample' : this.masked ? 'scalar' : 'none';
    const text = this.count ? formatCount(this.count, {stale: this.stale}) : this.masked ? formatMasked(this.masked, {stale: this.stale}) : '';
    const exact = this.count ? this.count.exact : this.masked ? this.masked.exact : false;
    return html`<span
        part="count"
        data-kind=${kind}
        data-exact=${exact ? 'true' : 'false'}
        data-empty=${text === '' ? 'true' : 'false'}
        >${text}</span
      >${this.label && text !== '' ? html`<span part="label">${this.label}</span>` : nothing}`;
  }
}

attachContextRoot();
defineOnce('tessera-count', TesseraCount);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-count': TesseraCount;
  }
}
