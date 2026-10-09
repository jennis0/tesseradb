import {ContextProvider} from '@lit/context';
import {css, html, nothing, type TemplateResult} from 'lit';
import type {Store} from '@mosaica/client';
import {MosaicaElement} from './base.js';
import {storeContext} from './context.js';
import {attachContextRoot, defineOnce} from './define.js';

/**
 * Provides one store, and so one view, to the panels and maps inside it, which find it by context.
 * An overview beside a detail is two of these. `<mosaica-explorer>` is its own provider and needs
 * none.
 *
 * Takes a `store` property, or builds a store from `viewer-url` and `token` or an `authorise`
 * property. It renders its children and nothing else (`display: contents`).
 *
 * @summary Provides one store to the elements inside it.
 * @tagname mosaica-store
 * @category Elements
 * @slot - The panels and maps that read the store.
 */
export class MosaicaStore extends MosaicaElement {
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
defineOnce('mosaica-store', MosaicaStore);

declare global {
  interface HTMLElementTagNameMap {
    'mosaica-store': MosaicaStore;
  }
}
