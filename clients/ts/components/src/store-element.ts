import {ContextProvider} from '@lit/context';
import {css, html, nothing, type TemplateResult} from 'lit';
import type {Store} from '@tesseradb/client';
import {TesseraElement} from './base.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';

/**
 * `<tessera-store>`: provides one store, and so one view, to the panels and maps inside it. An
 * overview beside a detail is two of these. The explorer is its own provider.
 *
 * Takes a `.store`, or builds one from `viewer-url` and `token` or `authorise`, with the same
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

  protected override onStoreAdopted(store: Store | null): void {
    this.provider.setValue(store);
  }

  override render(): TemplateResult | typeof nothing {
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
