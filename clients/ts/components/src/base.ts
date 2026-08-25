import {ContextConsumer} from '@lit/context';
import {LitElement, type PropertyValues} from 'lit';
import {property} from 'lit/decorators.js';
import {createStore, type Store, type TokenSupplier} from '@tesseradb/client';
import {storeContext} from './context.js';

/**
 * What every element shares: how it finds its store, and how it follows it.
 *
 * **Store precedence, decided synchronously at connection** (design §5.3, §5.9): a `.store`
 * property; else a context answer — the protocol's request is a one-shot event, so no answer at
 * connection means no provider, and a provider that connects *later* is adopted only if the
 * element built none; else, for the map and the explorer only, its own store from `viewer-url`
 * and `token` or an `authorise` property; else detached, which renders nothing rather than
 * "empty" or "refused", both of which are answers.
 *
 * **Never disposes the store on disconnect.** Frameworks reorder and keep-alive elements by
 * disconnecting and reconnecting them, and JupyterLab's windowed notebooks scroll cells out of
 * the DOM; a store torn down on each would refetch its whole view on every scroll-past. An own
 * store lives until `dispose()` is called on the element that built it.
 *
 * Following the store is one subscription over every projection; Lit coalesces the resulting
 * update requests into one render per microtask, and its templating keeps a control's DOM
 * identity across renders, which is what keeps a control from being rebuilt under the user.
 */
export type StoreSource = 'property' | 'context' | 'own' | 'detached';

export abstract class TesseraElement extends LitElement {
  /** A store handed in directly — the first rung of the precedence. */
  @property({attribute: false}) accessor store: Store | null = null;
  /** For an element that may build its own store (the map and the explorer). */
  @property({attribute: 'viewer-url'}) accessor viewerUrl = '';
  @property() accessor token = '';
  @property({attribute: false}) accessor authorise: TokenSupplier | null = null;

  /** Whether this element builds its own store from attributes when nothing else supplies one. */
  protected canBuildOwn = false;
  protected resolvedStore: Store | null = null;
  protected storeSource: StoreSource = 'detached';
  private ownStore: Store | null = null;
  private consumer: ContextConsumer<typeof storeContext, this> | null = null;
  private unsubscribe: (() => void) | null = null;

  /** The store this element reads, and where it came from — for a test, and for a host's probe. */
  get activeStore(): Store | null {
    return this.resolvedStore;
  }
  get source(): StoreSource {
    return this.storeSource;
  }

  override connectedCallback(): void {
    super.connectedCallback();
    this.resolveStore();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.unsubscribe?.();
    this.unsubscribe = null;
  }

  protected override willUpdate(changed: PropertyValues<this>): void {
    if (changed.has('store') && this.isConnected) {
      if (this.store) this.adopt(this.store, 'property');
    }
  }

  private resolveStore(): void {
    if (this.store) {
      this.adopt(this.store, 'property');
      return;
    }
    if (this.resolvedStore) {
      // A reconnect: keep what was resolved, and re-subscribe.
      this.subscribeTo(this.resolvedStore);
      return;
    }
    // The request goes out now, synchronously, and a provider above answers before this returns.
    this.consumer ??= new ContextConsumer(this, {
      context: storeContext,
      subscribe: true,
      callback: (value) => this.onContext(value)
    });
    if (this.resolvedStore) return;
    const own = this.buildOwn();
    if (own) {
      this.ownStore = own;
      this.adopt(own, 'own');
      return;
    }
    this.storeSource = 'detached';
  }

  /** Build a store from attributes — only where the element is allowed one and has enough to. */
  protected buildOwn(): Store | null {
    if (!this.canBuildOwn || !this.viewerUrl || (!this.token && !this.authorise)) return null;
    return createStore({
      viewerUrl: this.viewerUrl,
      ...(this.authorise ? {authorise: this.authorise} : {token: this.token})
    });
  }

  private onContext(value: Store | null | undefined): void {
    if (!value) return;
    // A property outranks context; an own store, once built, is kept — a provider that arrives
    // later does not take over a map that is already drawing.
    if (this.store || this.storeSource === 'own') return;
    this.adopt(value, 'context');
  }

  protected adopt(store: Store, source: StoreSource): void {
    if (store === this.resolvedStore && this.storeSource === source) return;
    this.resolvedStore = store;
    this.storeSource = source;
    this.subscribeTo(store);
    this.onStoreAdopted(store);
    this.onStoreChange();
    this.requestUpdate();
  }

  private subscribeTo(store: Store): void {
    this.unsubscribe?.();
    this.unsubscribe = store.subscribe(() => this.onStoreChange());
  }

  /** A hook for an element that wires more than a render to its store (the map, the provider). */
  protected onStoreAdopted(_store: Store): void {}

  /** Every projection change lands here; the default asks for a render. */
  protected onStoreChange(): void {
    this.requestUpdate();
  }

  /** Release the store this element built. A host that handed one in disposes it itself. */
  dispose(): void {
    this.unsubscribe?.();
    this.unsubscribe = null;
    if (this.ownStore) {
      this.ownStore.dispose();
      this.ownStore = null;
    }
    this.resolvedStore = null;
    this.storeSource = 'detached';
    this.requestUpdate();
  }
}

/** A custom event that bubbles through shadow roots, so a host listens on any ancestor (§5.7). */
export function emit(from: HTMLElement, name: string, detail: unknown): void {
  from.dispatchEvent(new CustomEvent(name, {detail, bubbles: true, composed: true}));
}

/** Ids cross the DOM boundary as decimal strings — the wire's own JSON form (§5.7). */
export function idString(id: bigint): string {
  return id.toString(10);
}
