import {createContext} from '@lit/context';
import type {Store} from '@mosaicajs/client';

/**
 * The Lit context the elements find their store by. `<mosaica-store>` and `<mosaica-explorer>`
 * provide it; a host with its own provider (a `ContextProvider` from `@lit/context`) provides a
 * store under this key. The key is `Symbol.for('mosaica.store')`, so two copies of the package
 * on one page share it.
 */
export const storeContext = createContext<Store | null>(Symbol.for('mosaica.store'));
