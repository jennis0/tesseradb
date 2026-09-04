import {createContext} from '@lit/context';
import type {Store} from '@tesseradb/client';

/**
 * The store, by context. Keyed with `Symbol.for` so two copies of the package on one page — the
 * unbundled and the single-file distribution, or a widget's `_esm` beside an app's — share one
 * key and a provider from either answers a consumer from either.
 */
export const storeContext = createContext<Store | null>(Symbol.for('tesseradb.store'));
