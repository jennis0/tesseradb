import {LitElement, css, html, nothing, type TemplateResult} from 'lit';
import {property} from 'lit/decorators.js';
import {formatCount, formatMasked, type Count, type Masked} from '@tesseradb/client';
import {attachContextRoot, defineOnce} from './define.js';
import {tokens} from './tokens.js';

/**
 * `<tessera-count>`: renders a `Count` or a `Masked` through the client's formatter. A sample shows
 * both figures or neither; a scalar one figure or none, marked approximate when inexact; nothing
 * against a stale view.
 *
 * `figure="shown"` renders a sample's shown figure alone, for a strip whose next cell is the
 * total, and carries the total on `data-total`. The cell is empty exactly when the pair would be.
 *
 * `part="count"` carries `data-kind` (`sample` or `scalar`), `data-exact`, and `data-empty` when
 * nothing was rendered.
 */
export class TesseraCount extends LitElement {
  static override styles = [
    tokens,
    css`
      :host {
        display: inline;
      }
      [part='count'] {
        color: var(--_tessera-ink);
        font-family: var(--_tessera-font-mono);
        font-variant-numeric: tabular-nums;
        font-weight: 500;
      }
      [part='label'] {
        color: var(--_tessera-ink-2);
        margin-left: 0.4em;
      }
    `
  ];

  @property({attribute: false}) accessor count: Count | null = null;
  @property({attribute: false}) accessor masked: Masked | null = null;
  @property({type: Boolean}) accessor stale = false;
  @property() accessor label = '';
  @property() accessor figure: 'both' | 'shown' = 'both';

  override render(): TemplateResult | typeof nothing {
    const kind = this.count ? 'sample' : this.masked ? 'scalar' : 'none';
    const pair = this.count ? formatCount(this.count, {stale: this.stale}) : this.masked ? formatMasked(this.masked, {stale: this.stale}) : '';
    const text = this.count && this.figure === 'shown' && pair !== '' ? pair.split(' of ')[0]! : pair;
    const total = this.count && this.figure === 'shown' && pair !== '' ? pair.split(' of ')[1] : undefined;
    const exact = this.count ? this.count.exact : this.masked ? this.masked.exact : false;
    return html`<span
        part="count"
        data-kind=${kind}
        data-exact=${exact ? 'true' : 'false'}
        data-empty=${text === '' ? 'true' : 'false'}
        data-total=${total ?? nothing}
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
