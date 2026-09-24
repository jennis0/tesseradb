import {createContext} from '@lit/context';
import type {Store} from '@tesseradb/client';

/**
 * The store, by context. Keyed with `Symbol.for` so two copies of the package on one page (the
 * unbundled and the single-file distribution, say) share the key and answer each other.
 */
export const storeContext = createContext<Store | null>(Symbol.for('tesseradb.store'));
