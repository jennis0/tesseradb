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
type OwnConfig = {viewerUrl: string; token: string; authorise: TokenSupplier | null};

/**
 * What every element shares: how it finds its store, and how it follows it.
 *
 * Store precedence: a `.store` property; else a context answer (a provider that connects after an
 * element built its own store is not adopted); else, for the map, the explorer and
 * `<tessera-store>`, its own store from `viewer-url` and `token` or an `authorise` property; else
 * detached, which renders nothing. An own store is built once those attributes suffice, and
 * replaced when `viewer-url`, `token` or the `authorise` function changes. A store handed in by
 * property or context is not disposed here.
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
   * expires. Setting another function builds a new store, since a store serves one viewer (see
   * `Store` in `@tesseradb/client`), so a framework keeps the function stable across renders.
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
  /** Whether the adopted store held `meta` at its last change, so a drop to `null` is seen. */
  private metaHeld = false;
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
      this.canBuildOwn && this.viewerUrl && (this.token || this.authorise) ? {viewerUrl: this.viewerUrl, token: this.token, authorise: this.authorise} : null;
    const held = this.ownConfig;
    const same = wanted !== null && held !== null && wanted.viewerUrl === held.viewerUrl && wanted.token === held.token && wanted.authorise === held.authorise;
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
          : createStore({viewerUrl: wanted.viewerUrl, ...(wanted.authorise ? {authorise: wanted.authorise} : {token: wanted.token})});
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
    this.metaHeld = store.get('meta') !== null;
    this.resetServerData();
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
    this.metaHeld = false;
    this.resetServerData();
    this.onStoreAdopted(null);
    this.requestUpdate();
  }

  private subscribeTo(store: Store): void {
    this.unsubscribe?.();
    this.unsubscribe = store.subscribe(() => {
      // `meta` goes to `null` when the store forgets what the server answered: a `clear()` or an
      // answer under another identity key.
      const held = store.get('meta') !== null;
      if (this.metaHeld && !held) this.resetServerData();
      this.metaHeld = held;
      this.onStoreChange();
    });
  }

  /** A hook for an element that wires more than a render to its store; `null` when it detaches. */
  protected onStoreAdopted(_store: Store | null): void {}

  /**
   * Forget what this element holds from the server, beyond what it reads from the store's
   * projections: fetched pages, names, a hover's record. Called when the element adopts or
   * detaches from a store, and when the store's `meta` goes to `null`, so nothing answered for one
   * viewer shows beside another's.
   */
  protected resetServerData(): void {}

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
 * What stands where an artifact has no name, on an element carrying `data-unnamed`, which draws it
 * faint. A key such as `hdb-2422486` is an identifier and would read as a name, so it shows only in
 * the card's key field.
 */
export const UNNAMED = 'Unnamed';

/** Emit one of the elements' events, bubbling and composed so it crosses shadow roots. */
export function emit<K extends keyof TesseraEventDetails>(from: HTMLElement, name: K, detail: TesseraEventDetails[K]): void {
  from.dispatchEvent(new CustomEvent(name, {detail, bubbles: true, composed: true}));
}

/** An id as the decimal string events carry, the wire's JSON form. */
export function idString(id: bigint): string {
  return id.toString(10);
}

/**
 * A column's caption: `submitted_at` reads as "Submitted at". A column declares no title, so its
 * name is written out as words.
 */
export function columnCaption(name: string): string {
  const words = name.replace(/_/g, ' ');
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/**
 * A category key's title where the store has one, from the legend's names or a suggestion page,
 * else the key.
 */
export function keyTitle(store: Store | null, column: string, key: string): string {
  if (!store) return key;
  const named = store.get('legend').categories[column]?.find((v) => v.key === key) ?? store.get('filters').suggestions[column]?.values.find((v) => v.key === key);
  return named?.title ?? key;
}

const DATE = new Intl.DateTimeFormat('en-GB', {day: 'numeric', month: 'long', year: 'numeric', timeZone: 'UTC'});
const pad = (n: number) => String(n).padStart(2, '0');

/**
 * A `timestamp_us` value in full, readably and in UTC: `14 March 2024, 12:00 UTC`, with seconds
 * where they are not zero. Fractions of a second are left out.
 */
export function timestampText(value: number | bigint): string {
  const d = new Date(Number(value) / 1000);
  const seconds = d.getUTCSeconds();
  return `${DATE.format(d)}, ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}${seconds ? `:${pad(seconds)}` : ''} UTC`;
}

/** A `timestamp_us` value as its UTC date alone, such as `14 March 2024`, where room is short. */
export function dateText(value: number | bigint): string {
  return DATE.format(new Date(Number(value) / 1000));
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
