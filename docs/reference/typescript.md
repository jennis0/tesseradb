<!-- The pages under typescript/ are generated from the doc comments in clients/ts. Edit the comments, then run: node clients/ts/scripts/reference.mjs -->

# TypeScript client

The TypeScript client is four packages in `clients/ts`. Each page below lists what a package exports for a host to use, generated from the doc comments in its source. Every name on a page is imported from that package, as in `import {createStore} from '@tesseradb/client'`.

| Package | What it holds |
|---|---|
| [`@tesseradb/client`](typescript/client.md) | The headless store that `createStore` returns, the viewer and session client `TesseraClient`, the operator's control-plane client `Control`, and the helpers the elements build their filters, pickers and counts with. |
| [`@tesseradb/deck`](typescript/deck.md) | `TesseraLayer`, a deck.gl layer that draws the store, for a host that builds its own `Deck`. deck.gl and luma.gl are peer dependencies. |
| [`@tesseradb/components`](typescript/components.md) | The types of the `tessera-*` elements' events, parts, context and theme. The elements are on the [Components](components.md) pages. |
| [`@tesseradb/react`](typescript/react.md) | `useTesseraStore` and `useProjection`, the store as React hooks. |
| [`@tesseradb/react/components`](typescript/react/components.md) | Every element as a React component with typed props and event handlers. |

Not built yet: the packages are not published to npm. `npm run build` in `clients/ts` builds each into its `dist/` directory, and `npm pack` in a package's directory makes a tarball of it.
