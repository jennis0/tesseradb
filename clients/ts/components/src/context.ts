import {createContext} from '@lit/context';
import type {Store} from '@tesseradb/client';

/**
 * The Lit context the elements find their store by. `<tessera-store>` and `<tessera-explorer>`
 * provide it; a host with its own provider (a `ContextProvider` from `@lit/context`) provides a
 * store under this key. The key is `Symbol.for('tesseradb.store')`, so two copies of the package
 * on one page share it.
 */
export const storeContext = createContext<Store | null>(Symbol.for('tesseradb.store'));
