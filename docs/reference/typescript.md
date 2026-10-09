<!-- The pages under typescript/ are generated from the doc comments in clients/ts. Edit the comments, then run: node clients/ts/scripts/reference.mjs -->

# TypeScript client

The TypeScript client is four packages in `clients/ts`. Each page below lists what a package exports for a host to use, generated from the doc comments in its source. Every name on a page is imported from that package, as in `import {createStore} from '@mosaicajs/client'`.

| Package | What it holds |
|---|---|
| [`@mosaicajs/client`](typescript/client.md) | The headless store that `createStore` returns, the viewer and session client `MosaicaClient`, the operator's control-plane client `Control`, the bulk read `RecordsRead`, the helpers that build the filter drafts and member clauses the store takes and say which layers it draws and colours by, and the count formatters. |
| [`@mosaicajs/deck`](typescript/deck.md) | `MosaicaLayer`, a deck.gl layer that draws the store, for a host that builds its own `Deck`. deck.gl and luma.gl are peer dependencies. |
| [`@mosaicajs/components`](typescript/components.md) | The types of the `mosaica-*` elements' events, parts, context and theme. The elements are on the [Components](components.md) pages. |
| [`@mosaicajs/react`](typescript/react.md) | `useMosaicaStore` and `useProjection`, the store as React hooks. |
| [`@mosaicajs/react/components`](typescript/react/components.md) | Every element as a React component with typed props and event handlers. |

`@mosaicajs/client/internal` and `@mosaicajs/deck/internal` hold what the other packages use beyond these entries. They are not a public API and are not listed here.

Not built yet: the packages are not published to npm. `npm run build` in `clients/ts` builds each into its `dist/` directory, and `npm pack` in a package's directory makes a tarball of it.
