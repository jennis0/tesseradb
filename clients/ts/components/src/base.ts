import {ContextConsumer} from '@lit/context';
import {LitElement, type PropertyValues} from 'lit';
import {property} from 'lit/decorators.js';
import {createStore, type SelectionShape, type Store, type TokenSupplier} from '@tesseradb/client';
import type {SelectionShapeDetail, TesseraEventDetails} from './events.js';
import {storeContext} from './context.js';

/**
 * Where an element's store came from: its `store` property, a `<tessera-store>` or
 * `<tessera-explorer>` above it (`context`), one it built itself from `viewer-url` and `token` or
 * `authorise` (`own`), or none (`detached`), in which case it renders its detached state.
 */
export type StoreSource = 'property' | 'context' | 'own' | 'detached';

/** What an own store was built from; a change in any of these rebuilds it. */
type OwnConfig = {viewerUrl: string; token: string; supplied: boolean};

/**
 * What every element shares: how it finds its store, and how it follows it.
 *
 * Store precedence: a `.store` property; else a context answer (a provider that connects after an
 * element built its own store is not adopted); else, for the map, the explorer and
 * `<tessera-store>`, its own store from `viewer-url` and `token` or an `authorise` property; else
 * detached, which renders nothing. An own store is built once those attributes suffice, and
 * replaced when `viewer-url` or `token` changes or `authorise` is set or cleared. A store handed in
 * by property or context is not disposed here.
 *
 * Disconnecting does not dispose the store. Frameworks disconnect and reconnect elements to
 * reorder them, and JupyterLab scrolls notebook cells out of the DOM; a new store each time would
 * refetch the view. An own store lives until `dispose()` or until its attributes change.
 */
export abstract class TesseraElement extends LitElement {
  /**
   * The store to read, which outranks a store from context and the element's own. The element does
   * not dispose a store it was given.
   */
  @property({attribute: false}) accessor store: Store | null = null;
  /**
   * The viewer plane's base URL. Only `<tessera-map>`, `<tessera-explorer>` and `<tessera-store>`
   * read it, to build their own store where no `store` property or context supplies one. Changing
   * it builds a new store.
   */
  @property({attribute: 'viewer-url'}) accessor viewerUrl = '';
  /**
   * A viewer token for the store the element builds from `viewer-url`. Only the map, the explorer
   * and `<tessera-store>` read it. Changing it builds a new store.
   */
  @property() accessor token = '';
  /**
   * A token supplier, used in place of `token`, which the store calls to renew the token before it
   * expires. The store calls whichever function is set at the time, so replacing the function (an
   * inline arrow in a framework's render) does not rebuild the store; setting or clearing it does.
   * A different principal needs a new `token`, a new `viewer-url` or a new element.
   */
  @property({attribute: false}) accessor authorise: TokenSupplier | null = null;

  /** Whether this element builds its own store from attributes when nothing else supplies one. */
  protected canBuildOwn = false;
  protected resolvedStore: Store | null = null;
  protected storeSource: StoreSource = 'detached';
  private ownStore: Store | null = null;
  private ownConfig: OwnConfig | null = null;
  /** The last store a provider answered with, adopted where nothing outranks it. */
  private contextStore: Store | null = null;
  private consumer: ContextConsumer<typeof storeContext, this> | null = null;
  private unsubscribe: (() => void) | null = null;

  /** The store this element reads, from whichever source won; `null` while detached. */
  get activeStore(): Store | null {
    return this.resolvedStore;
  }
  /** Where `activeStore` came from. */
  get source(): StoreSource {
    return this.storeSource;
  }

  override connectedCallback(): void {
    super.connectedCallback();
    // A reconnect keeps what was resolved; `resolve` then catches up with anything that changed
    // while disconnected.
    if (this.resolvedStore) this.subscribeTo(this.resolvedStore);
    // The request is synchronous: a provider above answers before this returns.
    this.consumer ??= new ContextConsumer(this, {
      context: storeContext,
      subscribe: true,
      callback: (value) => this.onContext(value)
    });
    this.resolve();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.unsubscribe?.();
    this.unsubscribe = null;
  }

  protected override willUpdate(changed: PropertyValues<this>): void {
    if (!this.isConnected) return;
    if (changed.has('store') || changed.has('viewerUrl') || changed.has('token') || changed.has('authorise')) this.resolve();
  }

  /**
   * Adopt the store the precedence names now. An own store already drawing is kept over a
   * provider that arrived later, and rebuilt when its attributes change. A replaced own store is
   * disposed after its successor is adopted, so nothing reads a disposed store.
   */
  private resolve(): void {
    const wanted: OwnConfig | null =
      this.canBuildOwn && this.viewerUrl && (this.token || this.authorise) ? {viewerUrl: this.viewerUrl, token: this.token, supplied: this.authorise !== null} : null;
    const held = this.ownConfig;
    const same = wanted !== null && held !== null && wanted.viewerUrl === held.viewerUrl && wanted.token === held.token && wanted.supplied === held.supplied;
    const old = this.ownStore;
    let next: Store | null = null;
    let source: StoreSource = 'detached';
    if (this.store) {
      next = this.store;
      source = 'property';
    } else if (wanted && (this.storeSource === 'own' || !this.contextStore)) {
      next =
        old && same
          ? old
          : createStore({viewerUrl: wanted.viewerUrl, ...(wanted.supplied ? {authorise: () => this.authorise!()} : {token: wanted.token})});
      source = 'own';
    } else if (this.contextStore) {
      next = this.contextStore;
      source = 'context';
    }
    this.ownStore = source === 'own' ? next : null;
    this.ownConfig = source === 'own' ? wanted : null;
    if (next) this.adopt(next, source);
    else this.detach();
    if (old && old !== this.ownStore) old.dispose();
  }

  private onContext(value: Store | null | undefined): void {
    this.contextStore = value ?? null;
    // A property outranks context; an own store, once built, is kept, so a provider that arrives
    // later does not take over a map that is already drawing.
    if (this.store || this.storeSource === 'own') return;
    if (value) this.adopt(value, 'context');
    else if (this.storeSource === 'context') this.detach();
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

  private detach(): void {
    if (!this.resolvedStore && this.storeSource === 'detached') return;
    this.unsubscribe?.();
    this.unsubscribe = null;
    this.resolvedStore = null;
    this.storeSource = 'detached';
    this.onStoreAdopted(null);
    this.requestUpdate();
  }

  private subscribeTo(store: Store): void {
    this.unsubscribe?.();
    this.unsubscribe = store.subscribe(() => this.onStoreChange());
  }

  /** A hook for an element that wires more than a render to its store; `null` when it detaches. */
  protected onStoreAdopted(_store: Store | null): void {}

  /** Every projection change lands here; the default asks for a render. */
  protected onStoreChange(): void {
    this.requestUpdate();
  }

  /**
   * Dispose of the store this element built and detach from it. Disconnecting does not do this, so
   * a host that removes an element for good calls it. A store handed in by property or context is
   * left for its owner to dispose.
   */
  dispose(): void {
    const own = this.ownStore;
    this.ownStore = null;
    this.ownConfig = null;
    this.detach();
    own?.dispose();
  }
}

/**
 * What stands where an artifact or a record has no name. A key such as `hdb-2422486` is an
 * identifier and would read as a name, so it shows only in the card's key field.
 */
export const UNNAMED = '\u2014';

/** Emit one of the elements' events, bubbling and composed so it crosses shadow roots. */
export function emit<K extends keyof TesseraEventDetails>(from: HTMLElement, name: K, detail: TesseraEventDetails[K]): void {
  from.dispatchEvent(new CustomEvent(name, {detail, bubbles: true, composed: true}));
}

/** An id as the decimal string events carry, the wire's JSON form. */
export function idString(id: bigint): string {
  return id.toString(10);
}

/** A `timestamp_us` value in full, as an ISO date-time. */
export function timestampText(value: number | bigint): string {
  return new Date(Number(value) / 1000).toISOString();
}

/** What the map's pick resolved to where it found no item: a miss, or a broken pick. */
export type PickOutcome =
  | {kind: 'miss'}
  | {kind: 'broken'; index: number; layer: string | null; hasIds: boolean; idCount: number}
  | null;

/** A selection shape as an event carries it, an artifact's id as a decimal string. */
export function shapeDetail(shape: SelectionShape | null): SelectionShapeDetail | null {
  if (!shape || shape.kind !== 'artifact') return shape;
  return {...shape, id: idString(shape.id)};
}
