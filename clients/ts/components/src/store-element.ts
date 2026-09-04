import {ContextProvider} from '@lit/context';
import {css, html} from 'lit';
import type {Store} from '@tesseradb/client';
import {TesseraElement} from './base.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';

/**
 * `<tessera-store>` — the provider, for panels and maps that share one view (design §5.3 tier
 * 3). A store owns one view — one driver, one presented frame — so this shares a *single* view
 * among its descendants; an overview beside a detail is two of these. Nothing else needs it and
 * the documentation does not lead with it: the explorer is its own provider.
 *
 * Takes a `.store`, or builds one from `viewer-url` and `token` or `authorise`, on the same
 * precedence as the map.
 */
export class TesseraStore extends TesseraElement {
  static override styles = css`
    :host {
      display: contents;
    }
  `;

  protected override canBuildOwn = true;
  private provider = new ContextProvider(this, {context: storeContext, initialValue: null});

  protected override onStoreAdopted(store: Store): void {
    this.provider.setValue(store);
  }

  override dispose(): void {
    super.dispose();
    this.provider.setValue(null);
  }

  override render() {
    return html`<slot></slot>`;
  }
}

attachContextRoot();
defineOnce('tessera-store', TesseraStore);

declare global {
  interface HTMLElementTagNameMap {
    'tessera-store': TesseraStore;
  }
}
