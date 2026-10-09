<!-- The pages under components/ are generated from the element sources in clients/ts/components. Edit the doc comments there, then run: node clients/ts/scripts/reference.mjs -->

# Components

The `mosaica-*` custom elements, in the `@mosaica/components` package, built with Lit. Each element's page lists its attributes, properties, methods, events, slots, parts and CSS custom properties, generated from its source.

| Element | What it is |
|---|---|
| [`<mosaica-explorer>`](components/mosaica-explorer.md) | The map and every panel, in a default layout. |
| [`<mosaica-map>`](components/mosaica-map.md) | The map canvas. |
| [`<mosaica-store>`](components/mosaica-store.md) | Provides one store to the elements inside it. |
| [`<mosaica-status>`](components/mosaica-status.md) | The state and the counts of the view, on one line. |
| [`<mosaica-count>`](components/mosaica-count.md) | A sample count or a masked count, formatted. |
| [`<mosaica-layer-picker>`](components/mosaica-layer-picker.md) | Which annotation layers the map draws. |
| [`<mosaica-view-picker>`](components/mosaica-view-picker.md) | Chooses the view. |
| [`<mosaica-key-picker>`](components/mosaica-key-picker.md) | Chooses the view within the current group. |
| [`<mosaica-filter-panel>`](components/mosaica-filter-panel.md) | The field cards, what they count, and the clauses applied. |
| [`<mosaica-field-card>`](components/mosaica-field-card.md) | One field's counts in view and overall, with its filter and highlight. |
| [`<mosaica-filter>`](components/mosaica-filter.md) | A field card's search box. |
| [`<mosaica-cluster-filter>`](components/mosaica-cluster-filter.md) | A layer's field card's search box. |
| [`<mosaica-selection>`](components/mosaica-selection.md) | The selected region's counts, marks and actions. |
| [`<mosaica-item-card>`](components/mosaica-item-card.md) | The selected item's fields, with Open and Copy id. |
| [`<mosaica-artifact-card>`](components/mosaica-artifact-card.md) | The opened artifact, with its lineage and its filter buttons. |
| [`<mosaica-hierarchy>`](components/mosaica-hierarchy.md) | A layer's hierarchy, browsed apart from the viewport. |
| [`<mosaica-colour-editor>`](components/mosaica-colour-editor.md) | Every value or cluster's colour, to search, choose and reset. |

[Events](components/events.md) lists every event with its `detail`, and [Theme tokens](components/tokens.md) every CSS custom property with its defaults.

## Loading the elements

`import '@mosaica/components'` defines every element. Each element also has its own entry, such as `import '@mosaica/components/map'`, which defines that element and the elements it renders. The map, the explorer, the field card, the field column, the colour editor and the artifact card import `@mosaica/deck`, which depends on deck.gl; the other elements do not. `@deck.gl/aggregation-layers` is a peer dependency as well, which `@mosaica/deck` imports only when a map first draws density as hexagons or contours. A host's bundler puts it in a chunk of its own, which a page that never draws those does not load.

A page with no build step loads the single-file bundle, `mosaica-components.js`, which holds Lit, deck.gl, its aggregation layers and the decode worker. `npm run bundle -w @mosaica/components` in `clients/ts` writes it to `components/dist/` with its subresource-integrity hash in `mosaica-components.js.sri`:

```html
<script type="module" src="./mosaica-components.js" integrity="sha384-..."></script>
```

A tag already defined keeps its first definition, so two copies of the package on one page do not clash.

## How an element finds its store

Every element but `<mosaica-count>` reads a store, and takes the first of these that it has:

1. its `store` property;
2. the store of the nearest `<mosaica-store>` or `<mosaica-explorer>` above it;
3. for `<mosaica-map>`, `<mosaica-explorer>` and `<mosaica-store>` only, a store it builds from its `viewer-url` attribute, its `token` attribute or `authorise` property, and its `artifacts-per-tile` attribute, the most artifacts one level of a drawn layer shows in one tile. Without `artifacts-per-tile` a drawn layer shows nothing, colouring by a layer has no colours, and the store says so. The store asks for artifacts by tile at map zoom + 2, the map's own zoom rounded down even when the world is smaller than the map, so a tile is 128 to 256 pixels across. It names at most 558 tiles in one request, the most a 3840 by 2160 screen touches at that depth, or the deployment's `max_tiles_per_request` where that is fewer. A larger screen is asked for at zoom + 1, then coarser, until its tiles fit. So the tiles in view times `artifacts-per-tile` bounds the artifacts one level draws.

With none, the element renders its detached state. An element that has built its own store keeps it when a provider appears above it later. A store the element built is replaced when `viewer-url`, `token`, `artifacts-per-tile` or the `authorise` function changes, and disposed by the element's `dispose()`. A store serves one viewer, as `Store` in the [TypeScript reference](typescript.md) sets out: to show another viewer, give the element a new `token`, a new `authorise` function, a new `viewer-url`, or a new element. Without one of these, the previous viewer's data stays on screen for as long as `Store` in the TypeScript reference says. An element also drops the pages, names and hover it fetched itself when it adopts another store and when its store forgets what the server answered. A store it was given is left for its owner to dispose. Disconnecting an element keeps its store, so moving it in the page does not fetch the view again.

## Events and styling

Every event is a `CustomEvent` that bubbles and is composed, so a host listens on the element or on any ancestor, including one outside `<mosaica-explorer>`.

An element's parts are styled with `::part()`. An element that renders another inside its shadow root forwards the inner element's parts under its name: through `<mosaica-explorer>`, the map's toolbar is `::part(map-controls)` and the item card's title is `::part(item-card-title)`. `PARTS` in `@mosaica/components` lists each element's parts, for a host that renders an element in its own shadow root and forwards them.
